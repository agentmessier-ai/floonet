//! Fan-out query client: query all trusted peers in parallel, merge results
//! tagged by machine_id, and report peers that did not answer — a silent
//! partial result lets a caller conclude "never discussed X" from a sleeping
//! peer.

use anyhow::Result;
use serde::Deserialize;
use std::time::Duration;

pub const PEER_TIMEOUT: Duration = Duration::from_secs(5);

/// How many peers are queried at once.
///
/// A peer answers `/v1/search` from the scan provider, which walks its whole
/// corpus, so an unbounded fan-out is a work amplifier whose factor is the
/// size of the trust store. Four is useful at the scale this is for — two or
/// three Macs — while making a larger trust store cost gradually rather than
/// all at once. It bounds concurrency only; every peer is still queried.
pub const MAX_CONCURRENT_PEERS: usize = 4;

/// Above this many peers, `--all` refuses rather than fanning out.
///
/// A cap on concurrency alone still lets one command cost N full scans; it only
/// spreads them over time. Past a handful of machines, "search everywhere" stops
/// being a reasonable default and becomes a decision someone should make on
/// purpose, so the caller is told to name peers instead. Not a hard maximum:
/// naming them explicitly always works, at any number.
pub const FANOUT_REFUSE_ABOVE: usize = 8;

#[derive(Debug, Clone)]
pub struct PeerAddr {
    pub device_id: String,
    pub name: String,
    pub addr: String, // host:port
    /// This peer's ed25519 public key, for verifying its response.
    ///
    /// `None` means its results are dropped. A trusted row without a key is a
    /// peer floonet cannot check, and the whole point of verifying responses is
    /// that being on the trusted list is not itself evidence about the bytes
    /// that came back.
    pub pubkey: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PeerHit {
    pub session_id: String,
    pub ts: Option<i64>,
    /// `default` for the same reason as `sidechain`: a peer on an older build
    /// sends no uuid, and that must read as "no address", not a parse error.
    #[serde(default)]
    pub uuid: Option<String>,
    pub excerpt: String,
    pub role: String,
    /// `default`, so a hit from a peer on an older build reads as "not a
    /// subagent" / "surface unknown" rather than failing to parse.
    #[serde(default)]
    pub sidechain: bool,
    #[serde(default)]
    pub surface: tp_core::turn::Surface,
}

#[derive(Debug, Clone)]
pub struct FanOutResult {
    /// All hits from all responding peers, tagged with which machine.
    pub hits: Vec<(String, PeerHit)>,
    /// Peers that answered (device_id → hit count).
    pub answered: Vec<(String, usize)>,
    /// Peers that timed out or failed, with the reason — never silently omitted.
    pub failed: Vec<(String, String)>,
    /// Peers that answered but flagged their own result as partial
    /// (device_id → their `degraded` message).
    pub peer_degraded: Vec<(String, String)>,
}

/// Everything a fan-out can narrow with, so the wire carries what the local
/// query does. A peer that is not told a narrowing answers a wider question
/// than it was asked, and an answer that reads as "this is what is there" is
/// the one thing the coverage contract forbids an incomplete answer to do.
#[derive(Debug, Clone, Default)]
pub struct PeerQuery {
    pub regex: bool,
    pub include_thinking: bool,
    pub folder: Option<String>,
    pub until_ms: Option<i64>,
    pub runtimes: Vec<String>,
}

impl PeerQuery {
    /// The narrowings the caller actually asked for, by wire name. Empty means
    /// an unnarrowed query, for which a peer that echoes nothing is correct.
    fn requested(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.regex {
            v.push("regex");
        }
        if self.include_thinking {
            v.push("include_thinking");
        }
        if self.folder.is_some() {
            v.push("folder");
        }
        if self.until_ms.is_some() {
            v.push("until_ms");
        }
        if !self.runtimes.is_empty() {
            v.push("runtimes");
        }
        v
    }

    fn query_string(&self) -> String {
        let mut s = String::new();
        if self.regex {
            s.push_str("&regex=1");
        }
        if self.include_thinking {
            s.push_str("&include_thinking=1");
        }
        if let Some(f) = &self.folder {
            s.push_str(&format!("&folder={}", urlencode(f)));
        }
        if let Some(u) = self.until_ms {
            s.push_str(&format!("&until_ms={u}"));
        }
        if !self.runtimes.is_empty() {
            s.push_str(&format!(
                "&runtimes={}",
                urlencode(&self.runtimes.join(","))
            ));
        }
        s
    }
}

/// One process-wide client (connection pool + TLS stack), built on first
/// use, so a fan-out does not redo the TLS setup per call.
fn shared_client() -> Result<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let client = reqwest::Client::builder()
        .timeout(PEER_TIMEOUT)
        .danger_accept_invalid_certs(true)
        .build()?;
    // A losing race just builds one extra client; the first writer's is kept.
    let _ = CLIENT.set(client.clone());
    Ok(client)
}

/// Query peers. `identity` signs each request (RFC 9421, see `auth`): a
/// peer's auth middleware rejects any non-loopback request without a valid
/// signature, so an unsigned fan-out reports total failure.
pub async fn query_peers(
    identity: &crate::Identity,
    peers: &[PeerAddr],
    q: &str,
    since_ms: i64,
    limit: usize,
    narrow: &PeerQuery,
) -> Result<FanOutResult> {
    // Self-signed, unpinned TLS (encrypt-only — see `tls` module doc); the
    // caller's ed25519 signature on every request is what actually
    // authenticates it, unaffected by the certificate.
    let client = shared_client()?;
    // Bounds concurrency without changing who is queried: every peer still gets
    // a handle, and each waits its turn for a permit.
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_PEERS));
    let mut hits = Vec::new();
    let mut answered = Vec::new();
    let mut failed = Vec::new();
    let mut peer_degraded = Vec::new();

    // Each handle carries its own device_id, rather than relying on
    // positional correspondence with `peers` — a peer whose *signing* fails
    // (below) never gets a handle at all, so a `peers.iter().zip(handles)`
    // pairing would silently misattribute every peer after it.
    type PeerAnswer = Result<(Vec<PeerHit>, Option<String>)>;
    let mut handles: Vec<(String, tokio::task::JoinHandle<PeerAnswer>)> = Vec::new();
    for peer in peers {
        let client = client.clone();
        let q = q.to_string();
        let addr = peer.addr.clone();
        // The signed target includes the query string; the digest is of the
        // actual body (empty for a GET). The nonce is fresh per peer per
        // query and rides in the query string, which the request signature
        // covers via `@query`, so it costs no round trip and an attacker
        // cannot choose it; the peer echoes it in its response signature,
        // binding that answer to this question.
        let nonce = {
            use rand::Rng;
            let bytes: [u8; 16] = rand::thread_rng().gen();
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let path_and_query = format!(
            "/v1/search?q={}&since_ms={}&limit={}{}&nonce={}",
            urlencode(&q),
            since_ms,
            limit,
            narrow.query_string(),
            nonce
        );
        // Fail closed: a peer whose key we do not have cannot be verified, so
        // its answer cannot be used. Reported as this peer failing, exactly like
        // a timeout — never as an empty result set, which would read as "nothing
        // was discussed there".
        let Some(peer_key) = peer.pubkey.clone() else {
            failed.push((
                peer.device_id.clone(),
                "no stored public key — cannot verify its response; re-pair".to_string(),
            ));
            continue;
        };
        // A signing failure must not abort the whole fan-out —
        // report it as this one peer failing and move on to the rest.
        let signed = match crate::auth::sign_request(identity, "GET", &path_and_query, b"", None) {
            Ok(s) => s,
            Err(e) => {
                failed.push((
                    peer.device_id.clone(),
                    format!("failed to sign request: {e:#}"),
                ));
                continue;
            }
        };
        let permits = permits.clone();
        let device_id_for_verify = peer.device_id.clone();
        // Moved in so the task can compare what was asked against what the
        // peer says it applied.
        let wanted = narrow.requested();
        let handle = tokio::spawn(async move {
            // Held for the duration of this peer's request. `acquire_owned`
            // rather than a scoped guard because the future outlives this loop.
            let _permit = permits.acquire_owned().await;
            let url = format!("https://{addr}{path_and_query}");
            let res = client
                .get(&url)
                .header(crate::auth::SIGNATURE_INPUT_HEADER, &signed.signature_input)
                .header(crate::auth::SIGNATURE_HEADER, &signed.signature)
                .header(crate::auth::CONTENT_DIGEST_HEADER, &signed.content_digest)
                .send()
                .await?;
            let status = res.status();
            // Headers must be read before the body is consumed.
            let hdr = |name: &str| {
                res.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string()
            };
            let (sig_input, sig, digest) = (
                hdr(crate::auth::SIGNATURE_INPUT_HEADER),
                hdr(crate::auth::SIGNATURE_HEADER),
                hdr(crate::auth::CONTENT_DIGEST_HEADER),
            );
            if !status.is_success() {
                anyhow::bail!("peer returned HTTP {status}");
            }
            let bytes = res.bytes().await?;

            // Verified against the key of the peer we asked — not any trusted
            // key — and against the nonce we sent. Not skippable: an unsigned
            // response is not a degraded answer but an unauthenticated one,
            // and its hits land directly in a coding agent's context.
            let vk = ed25519_dalek::VerifyingKey::from_bytes(
                &<[u8; 32]>::try_from(peer_key.as_slice())
                    .map_err(|_| anyhow::anyhow!("stored public key is not 32 bytes"))?,
            )?;
            if !crate::auth::verify_response(
                status.as_u16(),
                &bytes,
                &sig_input,
                &sig,
                &digest,
                &device_id_for_verify,
                &nonce,
                &vk,
            ) {
                anyhow::bail!("response signature did not verify — dropping this peer's results");
            }
            let res: serde_json::Value = serde_json::from_slice(&bytes)?;
            // Deserialize strictly: silently turning a schema mismatch into
            // "answered with 0 hits" is a false negative dressed as success.
            let hits: Vec<PeerHit> = match res.get("hits") {
                Some(h) => serde_json::from_value(h.clone())?,
                None => anyhow::bail!("peer response has no `hits` field"),
            };
            let mut degraded = res
                .get("degraded")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());

            // What the caller asked to narrow by, minus what this peer said
            // it applied. A build that predates a parameter ignores it
            // silently and echoes nothing, so the missing acknowledgement is
            // the signal. Folded into `degraded` because it is the same fact:
            // this answer is narrower than it looks.
            let applied: Vec<String> = res
                .get("applied")
                .and_then(|a| serde_json::from_value(a.clone()).ok())
                .unwrap_or_default();
            let ignored: Vec<&str> = wanted
                .iter()
                .copied()
                .filter(|k| !applied.iter().any(|a| a == k))
                .collect();
            if !ignored.is_empty() {
                let note = format!(
                    "did not apply {} — its build predates {} on the search wire, so its hits answer a WIDER query than you asked for",
                    ignored.join(", "),
                    if ignored.len() == 1 { "that filter" } else { "those filters" }
                );
                degraded = Some(match degraded {
                    Some(d) => format!("{d}; {note}"),
                    None => note,
                });
            }
            Ok((hits, degraded))
        });
        handles.push((peer.device_id.clone(), handle));
    }

    for (device_id, handle) in handles {
        match handle.await {
            Ok(Ok((peer_hits, degraded))) => {
                answered.push((device_id.clone(), peer_hits.len()));
                if let Some(d) = degraded {
                    // A peer that knowingly answered partially must not be
                    // reported as a clean success.
                    peer_degraded.push((device_id.clone(), d));
                }
                for h in peer_hits {
                    hits.push((device_id.clone(), h));
                }
            }
            // Keep why it failed: a permanently misconfigured peer (401) is
            // otherwise indistinguishable from a laptop that is asleep.
            Ok(Err(e)) => failed.push((device_id, e.to_string())),
            Err(e) => failed.push((device_id, format!("task failed: {e}"))),
        }
    }
    Ok(FanOutResult {
        hits,
        answered,
        failed,
        peer_degraded,
    })
}

/// Percent-encode a query value, over UTF-8 bytes rather than scalar values:
/// encoding 'é' as `%E9` rather than `%C3%A9` yields a string the peer's
/// query parser accepts and answers with zero hits — a silent false negative
/// counted as a successful answer.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Merge fan-out hits with local hits, tagging machine, and surface a partial
/// answer as such. Used by the CLI `--scope all`.
pub fn merge_hits(
    local: Vec<(String, PeerHit)>,
    fan: FanOutResult,
) -> (Vec<(String, PeerHit)>, Option<String>) {
    let mut all = local;
    all.extend(fan.hits);
    // Sort by ts desc (newest first), stable.
    all.sort_by_key(|(_, h)| std::cmp::Reverse(h.ts.unwrap_or(0)));
    // Both classes of incompleteness must surface: peers that did not answer,
    // and peers that answered but said their own result was partial.
    let mut notes = Vec::new();
    if !fan.failed.is_empty() {
        let detail: Vec<String> = fan
            .failed
            .iter()
            .map(|(id, why)| format!("{id} ({why})"))
            .collect();
        notes.push(format!(
            "{} peer(s) did not answer: {}",
            fan.failed.len(),
            detail.join(", ")
        ));
    }
    for (id, d) in &fan.peer_degraded {
        notes.push(format!("{id} reported: {d}"));
    }
    let degraded = if notes.is_empty() {
        None
    } else {
        Some(notes.join(" · "))
    };
    (all, degraded)
}

#[cfg(test)]
mod wire_compat {
    use super::*;

    /// A peer running a build older than the sidechain/surface fields sends
    /// hits without them. That must parse — and read as "not a subagent,
    /// surface unknown", never as a claim of `current` the peer did not make.
    #[test]
    fn a_hit_from_an_older_peer_still_parses_and_claims_nothing() {
        let h: PeerHit = serde_json::from_str(
            r#"{"session_id":"m/claude_code/x","ts":123,"excerpt":"e","role":"user"}"#,
        )
        .unwrap();
        assert!(!h.sidechain);
        assert_eq!(h.surface, tp_core::turn::Surface::Unknown);
    }
}
