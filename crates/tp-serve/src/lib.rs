//! The PEER adapter — HTTP requests in, operations out.
//!
//! `tp-net` keeps what `tp-app` depends on: identity, RFC 9421 auth, TLS,
//! probe, pairing primitives, the peer client, the rate limiter. The
//! route-to-operation mapping lives here because it is an adapter over `App`,
//! beside `cmd/` (prose for a person) and `mcp.rs` (JSON for a model), and an
//! adapter must sit above the operations layer, which `tp-net` does not. A
//! crate rather than a module of `tp` so the integration tests link a library
//! instead of spawning the binary.

pub mod local;

use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::routing::{get, post};
use axum::{Json, Router};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tp_core::retrieval::{Query, Scope};
use tp_net::auth::{verify_request, ChallengeStore};
use tp_net::identity::{hostname, Identity};
use tp_net::pairing::{self, Incoming};
use tp_net::ratelimit::RateLimiter;
use tp_net::wire::{hex_decode_pub, hex_encode, PairRequest, PingResponse};
use tp_search::Retrieval;

#[derive(Clone)]
pub struct AppState {
    /// The operations layer, shared. `Mutex` because `Db` holds a rusqlite
    /// `Connection`, which is not `Sync`.
    pub app: Arc<Mutex<tp_app::App>>,
    pub identity: Arc<Identity>,
    pub challenges: Arc<ChallengeStore>,
    /// Held beside the mutex rather than inside it: handlers clone this Arc
    /// into `spawn_blocking`, so a corpus scan never holds the database lock
    /// and a slow search does not queue in front of every ping and pairing
    /// request.
    pub retrieval: Arc<Retrieval>,
    /// Guards `/v1/pair/request`, the one route that answers strangers and
    /// then takes the same mutex every signed request needs.
    pub pair_limiter: Arc<RateLimiter>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PairRespond {
    pub device_id: String,
    pub accept: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub session_id: String,
    pub ts: Option<i64>,
    /// The source record's own id — the turn's address, where the runtime has
    /// one. Two turns can share a `ts`; a `uuid` does not. `default` so an
    /// older peer's silence reads as "no address", not a parse failure.
    #[serde(default)]
    pub uuid: Option<String>,
    pub excerpt: String,
    pub role: String,
    /// `default` on both: an older peer sends neither, and its silence reads
    /// as "not a subagent" / "surface unknown" — never "current".
    #[serde(default)]
    pub sidechain: bool,
    #[serde(default)]
    pub surface: tp_core::turn::Surface,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub machine_id: String,
    pub hits: Vec<SearchHit>,
    pub degraded: Option<String>,
    /// Which narrowing parameters this peer understood, echoed from the
    /// request. An older peer ignores an unknown query parameter silently, and
    /// nothing in the request can tell the client which kind of peer it
    /// reached — so the answer carries it. `default` makes an old peer's
    /// silence an empty list, which is the signal the fan-out needs to report
    /// that the scope was not applied.
    #[serde(default)]
    pub applied: Vec<String>,
}

/// Routes that mutate peer state. Empty by design: trust changes are a human
/// running `fl pair approve` against the database, and no network path to them
/// exists. The predicate stays because the order around it is the control — a
/// write route is refused before any question about the caller's address —
/// and adding one back means deciding how a human, not a socket, authorises it.
fn is_write_route(path: &str) -> bool {
    path.starts_with("/v1/pair/respond")
}

/// The only gate: every request past the bootstrap routes needs a
/// trusted-device signature, whatever address it arrives from.
async fn auth_middleware(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    let path = req.uri().path().to_string();
    // The signed target includes the query string; `path` alone is only for
    // route classification. The query is the entire input of a GET.
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| path.clone());
    // Unauthenticated by necessity: a peer introducing itself is not yet
    // trusted, so gating these on a signature could never pair.
    //   /v1/ping         — identity handshake, precedes trust
    //   /v1/challenge    — a nonce, worthless without a key that can sign it
    //   /v1/pair/request — the bootstrap
    // Safety rests on what they can do: at most a `pending_in` row, and
    // approval is a human writing the database. There is no network path to it.
    if path == "/v1/ping" || path == "/v1/challenge" || path == "/v1/pair/request" {
        return Ok(next.run(req).await);
    }
    // Write routes are denied first, before any question about the caller's
    // address: "is this request local?" must never answer "may this request
    // mutate trust?", or any process on the machine inherits the operator's
    // authority by connecting over 127.0.0.1. The ordering is what keeps the
    // next write route from being a hole by construction.
    if is_write_route(&path) {
        return Err(StatusCode::FORBIDDEN);
    }

    // There is no loopback exemption. The local CLI, MCP and panel open SQLite
    // directly and never make an HTTP request, so an exemption would guard no
    // one while admitting every other local process — including a sandboxed
    // one denied `~/.teleport` that can still reach loopback. Presence on an
    // interface is not identity.

    // Require a valid RFC 9421 signature from a trusted device. The body is
    // buffered first: `verify_request` hashes it and checks that against
    // `Content-Digest`, so a captured signed request cannot be replayed with a
    // substituted body.
    let headers = req.headers();
    let sig_input = headers
        .get(tp_net::auth::SIGNATURE_INPUT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let sig = headers
        .get(tp_net::auth::SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let digest = headers
        .get(tp_net::auth::CONTENT_DIGEST_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (Some(sig_input), Some(sig), Some(digest)) = (sig_input, sig, digest) else {
        // A pre-RFC 9421 peer sends `x-tp-sig` instead of
        // `Signature`/`Signature-Input`. It is rejected either way; the log
        // line keeps that from reading as an ordinary unsigned request.
        if headers.get("x-tp-sig").is_some() {
            tp_core::log_warn!("tp-net: rejected a request bearing legacy x-tp-sig headers from {addr} — that peer needs `fld` reinstalled (RFC 9421 migration)");
        }
        return Err(StatusCode::UNAUTHORIZED);
    };
    let method = req.method().as_str().to_string();

    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;

    let verified = verify_request(
        &method,
        &path_and_query,
        &bytes,
        &sig_input,
        &sig,
        &digest,
        |kid| lookup_trusted_pubkey(&st, kid),
    );
    let Some(verified) = verified else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    // Writes bind a single-use challenge into the signature's `nonce`, so a
    // valid signature implies the signer held that unspent nonce. The nonce is
    // consumed only after the signature verifies: consuming first would let an
    // unauthenticated observer burn it with a garbage signature. Unreachable
    // while `is_write_route` is empty; kept as defense in depth.
    if is_write_route(&path) {
        let Some(nonce) = &verified.nonce else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        if !st.challenges.consume(nonce) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    let req = Request::from_parts(parts, Body::from(bytes));
    Ok(next.run(req).await)
}

/// Cap on a buffered request body: the auth middleware must read it to verify
/// the digest. Well above any legitimate payload.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Bodies on `/v1/pair/request`, which is not covered by `MAX_BODY_BYTES`.
const MAX_PAIR_BODY_BYTES: usize = 4 * 1024;

/// Resolve a claimed `keyid` (a `device_id`) to that one trusted peer's
/// pubkey — `None` for anyone not already trusted. One lookup rather than a
/// scan over every trusted key, and the caller's identity falls out of it.
fn lookup_trusted_pubkey(st: &AppState, device_id: &str) -> Option<VerifyingKey> {
    let app = st.app.lock().unwrap();
    let row = app.machine(device_id).ok().flatten()?;
    if row.trust != "trusted" {
        return None;
    }
    let bytes: [u8; 32] = row.pubkey?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn ping(State(st): State<AppState>) -> Json<PingResponse> {
    Json(PingResponse {
        device_id: st.identity.device_id.clone(),
        name: hostname(),
        // The build, not just the semver, so a human can compare two
        // machines. `/v1/ping` is unauthenticated, so this tells a LAN
        // stranger the exact build — accepted: the endpoint already discloses
        // device_id, hostname and public key, and diagnosing fan-out to a
        // peer is what the field is for.
        version: tp_core::VERSION_LINE.to_string(),
        pubkey: hex_encode(st.identity.verifying.as_bytes()),
    })
}

async fn pair_request(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<PairRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Before the ed25519 work, the expensive part of answering a stranger.
    // Loopback is not exempt: this is a resource decision, not a trust one,
    // and the local CLI never sends this route to its own machine.
    if !st.pair_limiter.allow(peer.ip()) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    // A name that can repaint the operator's terminal is a 400, not a 500:
    // `upsert_peer` refuses it too, but as a backstop, and an `Err` there
    // would blame this machine for a fault in what the caller sent.
    if !pairing::name_is_displayable(&body.name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let Some(bytes) = hex_decode_pub(&body.pubkey) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let arr: [u8; 32] = bytes.try_into().map_err(|_| StatusCode::BAD_REQUEST)?;
    let pubkey = VerifyingKey::from_bytes(&arr).map_err(|_| StatusCode::BAD_REQUEST)?;
    // The claimed device_id must be the pubkey's own fingerprint. Otherwise an
    // attacker can register a device_id that spoofs a real machine under a key
    // they control, defeating out-of-band fingerprint comparison at approval.
    if body.device_id != tp_net::identity::fingerprint(&pubkey) {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Observed IP + stated listen port, so an approved peer is reachable
    // immediately instead of waiting for mDNS.
    let addr = format!("{}:{}", peer.ip(), body.port);
    let app = st.app.lock().unwrap();
    let res = app
        .record_incoming_pairing(&body.device_id, &body.name, &pubkey, Some(&addr))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    drop(app);
    match res {
        Incoming::Recorded(res) => Ok(Json(
            serde_json::json!({ "status": format!("{:?}", res.status) }),
        )),
        Incoming::ListFull => {
            // The operator has to be told, because the fix is theirs and
            // nothing else surfaces it: to the peer this is a 503, and here it
            // is a request that never appears in `fl pair list` — the outcome
            // an attacker filling the list is going for.
            tp_core::log_warn!(
                "tp-net: refused a pairing request from {addr} — {} pending requests already, \
                 the cap. Clear them with `fl pair reject <device-id>`; until then no new peer \
                 can pair with this machine.",
                pairing::MAX_PENDING_IN
            );
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

async fn challenge(State(st): State<AppState>) -> Json<serde_json::Value> {
    let nonce = st.challenges.issue();
    Json(serde_json::json!({ "challenge": nonce }))
}

/// Ceiling on the rows one request may ask for. A trusted peer is still a
/// remote caller, and `limit` is what sizes the answer: unbounded, a single
/// request becomes a scan of the whole corpus collected in memory.
const MAX_LIMIT: usize = 500;

/// The caller's `limit`, or `default` when it asked for none. A value this
/// server will not honour as written is refused rather than reduced: a smaller
/// answer to a larger question still reads as "this is what is there".
fn limit_param(
    params: &std::collections::HashMap<String, String>,
    default: usize,
) -> Result<usize, StatusCode> {
    let Some(raw) = params.get("limit") else {
        return Ok(default);
    };
    match raw.parse::<usize>() {
        Ok(v) if (1..=MAX_LIMIT).contains(&v) => Ok(v),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

async fn search(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<SearchResponse>, StatusCode> {
    // An absent or empty needle matches every line of every transcript, which
    // would make the widest read this server can perform the easiest request to
    // make by accident. The caller must say what it is looking for.
    let Some(q_text) = params.get("q").filter(|t| !t.is_empty()).cloned() else {
        return Err(StatusCode::BAD_REQUEST);
    };
    // One type for both bounds — the same concept, a moment in unix ms.
    // Negative is refused, not defaulted: a bound before the epoch is a
    // mistake, and answering the unbounded question for it is a silent wrong
    // answer.
    let since_ms: i64 = match params.get("since_ms").and_then(|s| s.parse().ok()) {
        Some(v) if v < 0 => return Err(StatusCode::BAD_REQUEST),
        Some(v) => v,
        None => 6 * 3600 * 1000,
    };
    let until_ms: Option<i64> = match params.get("until_ms").and_then(|s| s.parse().ok()) {
        Some(v) if v < 0 => return Err(StatusCode::BAD_REQUEST),
        other => other,
    };
    let limit = limit_param(&params, 20)?;

    // Every narrowing the caller can express locally is honoured here.
    // Dropping `regex` returns matches the caller did not ask for; dropping
    // `folder` or `until` returns a different corpus. Both read as "this is
    // what is there".
    let flag = |k: &str| params.get(k).is_some_and(|v| v == "1" || v == "true");
    let query = Query {
        text: q_text.clone(),
        regex: flag("regex"),
        include_thinking: flag("include_thinking"),
        limit,
    };
    let scope = Scope {
        folder: params.get("folder").cloned(),
        since: Duration::from_millis(since_ms as u64),
        runtimes: params
            .get("runtimes")
            .map(|v| {
                v.split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        until: until_ms,
    };
    // What this build understood *and acted on*, echoed back: an older peer
    // ignores an unknown parameter silently, so the client learns from the
    // response which narrowings shaped the answer. Read from the parsed query
    // and scope rather than from which keys arrived — a parameter that failed
    // to parse narrowed nothing, and reporting it as applied tells the caller
    // its answer is narrower than it is.
    let applied: Vec<String> = [
        ("regex", query.regex),
        ("include_thinking", query.include_thinking),
        ("folder", scope.folder_needle().is_some()),
        ("until_ms", scope.until.is_some()),
        ("runtimes", !scope.runtimes.is_empty()),
    ]
    .into_iter()
    .filter(|(_, took_effect)| *took_effect)
    .map(|(name, _)| name.to_string())
    .collect();
    // `Retrieval::search` is a synchronous walk of the whole transcript corpus.
    // Inline it would park a tokio worker for its duration, and a worker that
    // cannot be cancelled keeps burning after the peer times out and retries.
    let retrieval = st.retrieval.clone();
    let (q2, s2) = (query.clone(), scope.clone());
    let got = tokio::task::spawn_blocking(move || retrieval.search(&q2, &s2))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let hits = got
        .items
        .into_iter()
        .map(|h| {
            let excerpt = h.excerpt().to_string();
            SearchHit {
                session_id: h.at.session_id,
                ts: h.at.ts,
                uuid: h.at.uuid,
                excerpt,
                role: format!("{:?}", h.role).to_lowercase(),
                sidechain: h.sidechain,
                surface: h.surface,
            }
        })
        .collect();
    Ok(Json(SearchResponse {
        applied,
        machine_id: st.identity.device_id.clone(),
        hits,
        degraded: got.coverage.degraded,
    }))
}

/// Known peers + trust state (drives `fl pair list`).
async fn machines(State(st): State<AppState>) -> Json<serde_json::Value> {
    let app = st.app.lock().unwrap();
    let peers = app.trusted_peers().unwrap_or_default();
    drop(app);
    Json(serde_json::json!({
        "machines": peers.iter().map(|p| {
            serde_json::json!({
                "id": p.id, "name": p.name, "trust": p.trust, "last_seen_at": p.last_seen_at
            })
        }).collect::<Vec<_>>()
    }))
}

/// Goes through `Retrieval`, not `tp_db::query`: the funnel scrubs session
/// titles (user prompt text) before they leave the machine and honours the
/// configured scan/index strategy.
async fn sessions(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // As in `search`: a moment in unix ms, and a negative one is refused
    // rather than defaulted.
    let since_ms: i64 = match params.get("since_ms").and_then(|s| s.parse().ok()) {
        Some(v) if v < 0 => return Err(StatusCode::BAD_REQUEST),
        Some(v) => v,
        None => 7 * 24 * 3600 * 1000,
    };
    let limit = limit_param(&params, 50)?;
    let scope = Scope {
        folder: None,
        since: Duration::from_millis(since_ms as u64),
        runtimes: vec![],
        until: None,
    };
    // As in `search`: parsing every candidate transcript is blocking work.
    let retrieval = st.retrieval.clone();
    let s2 = scope.clone();
    let got = tokio::task::spawn_blocking(move || retrieval.sessions(&s2, limit))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "sessions": got.items.iter().map(|s| {
            serde_json::json!({
                "id": s.id,
                "cwd": s.cwd,
                "title": s.title,          // scrubbed by the Retrieval funnel
                "last_turn_at": s.last_turn_at,
                "turn_count": s.turn_count
            })
        }).collect::<Vec<_>>(),
        "degraded": got.coverage.degraded,
    })))
}

// ── Construction / serve ─────────────────────────────────────────────────────

/// The `nonce` query parameter, decoded, matched as a whole parameter name.
/// Scanning the raw query for a `nonce=` prefix yields a still-encoded value
/// the caller never sent, and the answer then verifies as though unsigned.
fn nonce_param(query: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (decode_query_component(k) == "nonce").then(|| decode_query_component(v))
    })
}

/// Percent-decoding as the extractor that reads the same query string for the
/// handlers performs it, `+` for space included, so one request cannot be read
/// two ways.
fn decode_query_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 3 <= bytes.len() => {
                match (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi << 4) | lo);
                        i += 3;
                    }
                    // Not an escape after all: the '%' stands for itself.
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Sign every response this server sends. A middleware rather than
/// per-handler code, so "which responses are signed" is not a per-route
/// decision anyone can forget: a fan-out response lands in a coding agent's
/// context, so an unverified `hits` array is a prompt-injection channel.
///
/// The signature carries the caller's `nonce` query parameter, which the
/// request signature already covered via `@query`, binding this answer to that
/// question: a captured response cannot be replayed against a different one.
async fn sign_response_middleware(
    State(st): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    // Read the nonce before the request is consumed. Absent is fine: an older
    // client sends none and is not verifying either.
    let nonce = req.uri().query().and_then(nonce_param);

    let res = next.run(req).await;
    let (mut parts, body) = res.into_parts();
    let status = parts.status.as_u16();
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    if let Ok(signed) = tp_net::auth::sign_response(&st.identity, status, &bytes, nonce.as_deref())
    {
        for (name, value) in [
            (tp_net::auth::SIGNATURE_INPUT_HEADER, signed.signature_input),
            (tp_net::auth::SIGNATURE_HEADER, signed.signature),
            (tp_net::auth::CONTENT_DIGEST_HEADER, signed.content_digest),
        ] {
            if let Ok(v) = axum::http::HeaderValue::from_str(&value) {
                parts.headers.insert(name, v);
            }
        }
    }
    Ok(axum::response::Response::from_parts(
        parts,
        Body::from(bytes),
    ))
}

pub(crate) fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/ping", get(ping))
        .route("/v1/challenge", get(challenge))
        .route(
            "/v1/pair/request",
            // The auth middleware caps bodies at MAX_BODY_BYTES and this route
            // returns before reaching it, so without this its only bound would
            // be axum's default — on the one endpoint that reads a body from
            // someone unauthenticated.
            post(pair_request).layer(DefaultBodyLimit::max(MAX_PAIR_BODY_BYTES)),
        )
        .route("/v1/search", get(search))
        .route("/v1/sessions", get(sessions))
        .route("/v1/machines", get(machines))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        // Outside the auth layer, so it also signs the 401s and 403s that
        // layer produces: a client must be able to tell "this peer refused
        // me" from "something in the middle refused for it".
        .layer(middleware::from_fn_with_state(
            state.clone(),
            sign_response_middleware,
        ))
        .with_state(state)
}

/// Bind and spawn the server over TLS. Returns the actual bound address.
pub async fn serve(state: AppState, addr: SocketAddr) -> std::io::Result<SocketAddr> {
    let app = build_router(state);
    let (cert_pem, key_pem) =
        tp_net::tls::self_signed_pem().map_err(|e| std::io::Error::other(format!("{e:#}")))?;
    let config = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem)
        .await
        .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let bound = listener.local_addr()?;
    tokio::spawn(async move {
        // The daemon outlives its listener otherwise: still running, answering
        // nothing, with no record of what stopped it.
        if let Err(e) = axum_server::from_tcp_rustls(listener, config)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await
        {
            tp_core::log_error!("tp-serve: the peer listener stopped serving: {e}");
        }
    });
    Ok(bound)
}

/// Convenience for building the default local state (used by the CLI).
pub fn state_from(app: tp_app::App) -> AppState {
    let identity = Arc::new(app.identity().clone());
    let retrieval = app.retrieval();
    AppState {
        app: Arc::new(Mutex::new(app)),
        identity,
        challenges: Arc::new(ChallengeStore::new()),
        retrieval,
        pair_limiter: Arc::new(RateLimiter::new()),
    }
}
