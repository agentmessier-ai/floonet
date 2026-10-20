//! Exempt: test output goes to the test harness, which is the point of it.

//! Server integration: two local servers pair over HTTP, and the bootstrap
//! routes behave the same from loopback as from the network. The signature
//! path itself is covered by the fan-out tests.

use std::net::SocketAddr;
use tp_db::Db;
use tp_net::Identity;
use tp_serve::{serve, state_from, AppState};

fn base_url(addr: SocketAddr) -> String {
    format!("https://127.0.0.1:{}", addr.port())
}

fn empty_retrieval() -> tp_search::Retrieval {
    tp_search::Retrieval::new(Box::new(tp_search::ScanProvider::new(
        "test-machine",
        vec![Box::new(tp_ingest::builtin("claude_code"))],
        vec![(
            "claude_code".to_string(),
            "/nonexistent-floonet-test-root".into(),
        )],
    )))
}

fn make_state(identity: Identity) -> AppState {
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&identity.device_id, "TestMac")
        .unwrap();
    state_from(tp_app::App::from_parts(db, identity, empty_retrieval()))
}

fn hex_public(id: &Identity) -> String {
    id.verifying
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_flow_over_http() {
    let a = Identity::generate();
    let b = Identity::generate();

    let state_a = make_state(a.clone());
    let addr_a = serve(state_a.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr_b = serve(make_state(b.clone()), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let url_a = base_url(addr_a);
    let url_b = base_url(addr_b);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    // Ping (unauthenticated handshake).
    let pa: serde_json::Value = client
        .get(format!("{url_a}/v1/ping"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pa["device_id"].as_str().unwrap(), a.device_id);

    // A to B: B records A as pending_in.
    let res = client
        .post(format!("{url_b}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": a.device_id, "name": "machine-a", "pubkey": hex_public(&a), "port": 47400
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "pair request must succeed");

    // B to A: A records B as pending_in (mirror side of the handshake).
    client
        .post(format!("{url_a}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": b.device_id, "name": "machine-b", "pubkey": hex_public(&b), "port": 47400
        }))
        .send()
        .await
        .unwrap();

    // `/v1/challenge` is a bootstrap route and answers pre-auth.
    let _ch: serde_json::Value = client
        .get(format!("{url_a}/v1/challenge"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Approval happens the way it actually happens: a local write to the
    // database. There is no HTTP route for it.
    {
        let app = state_a.app.lock().unwrap();
        app.pair_decide(&b.device_id, true).unwrap();
    }

    // A now lists B as trusted. Asserted against the database: the fact being
    // checked is the row, and reading it over HTTP would need a signature.
    {
        let app = state_a.app.lock().unwrap();
        let trusted = app.trusted_peers().unwrap();
        assert_eq!(trusted.len(), 1, "A must trust exactly B");
        assert_eq!(trusted[0].id, b.device_id);
    }
}

/// Loopback must not be able to mutate trust: any local process could
/// otherwise POST accept:true and make an attacker-controlled machine
/// permanently trusted with no human and no signature. The route is absent,
/// and write routes are refused before any question about the caller's
/// address; this guards that order.
#[tokio::test(flavor = "multi_thread")]
async fn loopback_cannot_mutate_trust() {
    let a = Identity::generate();
    let b = Identity::generate();
    let state_a = make_state(a.clone());
    let addr_a = serve(state_a.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let url_a = base_url(addr_a);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    client
        .post(format!("{url_a}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": b.device_id, "name": "machine-b", "pubkey": hex_public(&b), "port": 47400
        }))
        .send()
        .await
        .unwrap();
    let res = client
        .post(format!("{url_a}/v1/pair/respond"))
        .json(&serde_json::json!({ "device_id": b.device_id, "accept": true }))
        .send()
        .await
        .unwrap();
    assert!(
        res.status() != 200,
        "a local process must not be able to grant permanent remote trust; got {}",
        res.status()
    );

    // And it did not take effect by some other path either.
    {
        let app = state_a.app.lock().unwrap();
        assert!(
            app.trusted_peers().unwrap().is_empty(),
            "no machine may become trusted without a human running `fl pair approve`"
        );
    }
}

/// `/v1/pair/request` must be reachable by an untrusted peer over the
/// network: it is the only way a new device introduces itself, and requiring
/// a signature there deadlocks pairing.
#[tokio::test(flavor = "multi_thread")]
async fn pair_request_is_reachable_from_a_non_loopback_address() {
    let host = Identity::generate();
    let stranger = Identity::generate();

    // Bind on all interfaces so the request arrives from a LAN address.
    let state = make_state(host.clone());
    let addr = serve(state.clone(), "0.0.0.0:0".parse().unwrap())
        .await
        .unwrap();
    let Some(lan_ip) = local_ipv4() else {
        eprintln!("no non-loopback IPv4 on this host; skipping");
        return;
    };
    let url = format!("https://{lan_ip}:{}", addr.port());
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    // A protected route from the same address must be rejected, which proves
    // the request really arrives as non-loopback.
    let protected = client
        .get(format!("{url}/v1/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        protected.status(),
        401,
        "non-loopback reads must require a signature"
    );

    // The bootstrap endpoint must still accept the introduction.
    let res = client
        .post(format!("{url}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": stranger.device_id, "name": "stranger", "pubkey": hex_public(&stranger), "port": 47400
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        200,
        "an untrusted peer must be able to introduce itself"
    );
    let json: serde_json::Value = res.json().await.unwrap();
    assert_eq!(json["status"].as_str().unwrap(), "PendingIn");

    // Only pending: it must not be trusted without approval.
    {
        let app = state.app.lock().unwrap();
        assert!(
            app.trusted_peers().unwrap().is_empty(),
            "a self-introduced peer must NOT be trusted until a human approves it"
        );
    }
}

fn local_ipv4() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("10.255.255.255:1").ok()?;
    let ip = s.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip.to_string())
    }
}

/// Loopback must not read either. The local CLI, MCP and panel open SQLite
/// directly and never make an HTTP request, so a loopback exemption would
/// admit only other local processes — including a sandboxed one denied
/// `~/.teleport` with loopback still open. Transcripts are the whole content
/// of this database; an unauthenticated local read is a quieter hole than a
/// write, not a smaller one.
#[tokio::test(flavor = "multi_thread")]
async fn loopback_cannot_read_without_a_signature() {
    let host = Identity::generate();
    let addr = serve(make_state(host), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let url = base_url(addr);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    for route in ["/v1/sessions", "/v1/machines", "/v1/search?q=x"] {
        let res = client.get(format!("{url}{route}")).send().await.unwrap();
        assert_eq!(
            res.status(),
            401,
            "{route} must require a signature even from loopback"
        );
    }

    // The bootstrap routes stay open, or pairing deadlocks.
    let res = client.get(format!("{url}/v1/ping")).send().await.unwrap();
    assert_eq!(res.status(), 200, "/v1/ping is the pairing bootstrap");
}

/// A name that can repaint the operator's terminal is the caller's fault, so
/// it comes back 400 — not the 500 that `upsert_peer`'s backstop would
/// produce if the handler let it through.
#[tokio::test(flavor = "multi_thread")]
async fn a_pair_request_with_a_terminal_escape_in_its_name_is_refused() {
    let host = Identity::generate();
    let stranger = Identity::generate();
    let state = make_state(host);
    let addr = serve(state.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let url = base_url(addr);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client
        .post(format!("{url}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": stranger.device_id,
            "name": "innocent\u{1b}[2K\rmachine-b   trusted",
            "pubkey": hex_public(&stranger),
            "port": 47400
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        400,
        "the name is the caller's fault, not ours"
    );

    let app = state.app.lock().unwrap();
    assert!(
        app.machine(&stranger.device_id).unwrap().is_none(),
        "nothing may be stored for a refused name"
    );
}

/// Answering a stranger costs an ed25519 decompression and then the same
/// mutex every signed request takes, so without a limit the introduction
/// endpoint is a lever on the traffic of peers already trusted. Every request
/// here arrives from one address, so the per-address bucket empties long
/// before `MAX_PENDING_IN` rows could accumulate; the cap is exercised at the
/// `pairing` level instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_flood_of_pair_requests_from_one_address_is_cut_off() {
    let host = Identity::generate();
    let state = make_state(host);
    let addr = serve(state.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let url = base_url(addr);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let mut refused = 0;
    let mut accepted = 0;
    // Each one is a distinct keypair, because the table is keyed on the
    // fingerprint: repeating one identity would be an upsert.
    for _ in 0..20 {
        let s = Identity::generate();
        let res = client
            .post(format!("{url}/v1/pair/request"))
            .json(&serde_json::json!({
                "device_id": s.device_id, "name": "flood", "pubkey": hex_public(&s), "port": 47400
            }))
            .send()
            .await
            .unwrap();
        match res.status().as_u16() {
            200 => accepted += 1,
            429 => refused += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert!(refused > 0, "a burst of 20 must not be served in full");
    let app = state.app.lock().unwrap();
    let stored = app
        .peers()
        .unwrap()
        .iter()
        .filter(|m| m.trust == "pending_in")
        .count();
    assert_eq!(
        stored, accepted,
        "every accepted request should be exactly one row, and no refused one"
    );
}

/// The body cap on this route is easy to delete by accident, because the auth
/// middleware's `MAX_BODY_BYTES` looks like it already covers everything —
/// and this is the one route that returns before reaching it.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_pair_request_body_is_refused_before_it_is_parsed() {
    let host = Identity::generate();
    let stranger = Identity::generate();
    let state = make_state(host);
    let addr = serve(state, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let url = base_url(addr);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    // Both outcomes are the cap working: if the server refuses before the
    // client has finished writing, the write fails with EPIPE and there is no
    // status to read. A refusal that arrives early is still a refusal.
    let sent = client
        .post(format!("{url}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": stranger.device_id,
            "name": "x".repeat(1024 * 1024),
            "pubkey": hex_public(&stranger),
            "port": 47400
        }))
        .send()
        .await;
    match sent {
        Ok(res) => assert_eq!(
            res.status(),
            413,
            "a megabyte from an unauthenticated caller must not be buffered"
        ),
        Err(e) => assert!(
            e.is_request(),
            "the only acceptable failure is the server hanging up mid-body; got {e:?}"
        ),
    }
}
