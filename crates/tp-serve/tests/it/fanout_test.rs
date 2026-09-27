//! Exempt: test output goes to the test harness, which is the point of it.

//! Fan-out: query a real local server (with an actual indexed fixture) through
//! the peer client, and verify a dead peer is reported, never silently dropped.

use std::path::Path;
use tp_db::Db;
use tp_net::peer::{merge_hits, query_peers, PeerAddr};
use tp_net::Identity;
use tp_serve::{serve, state_from, AppState};

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp(ms / 1000, ((ms % 1000) * 1_000_000) as u32)
        .unwrap()
        .to_rfc3339()
}

/// Build a scan retrieval rooted at a fixture dir containing one session.
fn retrieval_with_fixture(root: &std::path::Path) -> tp_search::Retrieval {
    // The scan provider reads from the fixture root.
    let _ = std::fs::create_dir_all(root.join("-Users-test-dev-demo"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let line = serde_json::json!({
        "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": rfc3339(now),
        "message": {"content": "the purple unicorn flies at dawn"}
    });
    std::fs::write(
        root.join("-Users-test-dev-demo")
            .join("ffffffff-1111-2222-3333-444444444444.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    tp_search::Retrieval::new(Box::new(tp_search::ScanProvider::new(
        "test-machine",
        vec![Box::new(tp_ingest::builtin("claude_code"))],
        vec![("claude_code".to_string(), root.to_path_buf())],
    )))
}

/// A peer state that trusts `caller`, which is what a real one does before it
/// will answer anything: there is no loopback exemption to lean on.
fn make_state_trusting(identity: Identity, root: &Path, caller: &Identity) -> AppState {
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&identity.device_id, "TestMac")
        .unwrap();
    tp_net::pairing::request_out(
        db.conn(),
        &caller.device_id,
        "caller",
        &caller.verifying,
        None,
    )
    .unwrap();
    tp_net::pairing::approve(db.conn(), &caller.device_id).unwrap();
    state_from(tp_app::App::from_parts(
        db,
        identity,
        retrieval_with_fixture(root),
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn fan_out_returns_hits_from_live_peer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let peer_ident = Identity::generate();
    let client_id = Identity::generate();
    let addr = serve(
        make_state_trusting(peer_ident.clone(), &root, &client_id),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let peers = vec![PeerAddr {
        device_id: peer_ident.device_id.clone(),
        name: "peer".into(),
        addr: format!("127.0.0.1:{}", addr.port()),
        pubkey: Some(peer_ident.verifying.to_bytes().to_vec()),
    }];
    let fan = query_peers(
        &client_id,
        &peers,
        "purple unicorn",
        3_600_000,
        10,
        &Default::default(),
    )
    .await
    .unwrap();
    assert!(
        fan.failed.is_empty(),
        "the live peer must answer, not fail: {:?}",
        fan.failed
    );
    assert_eq!(fan.answered.len(), 1);
    assert_eq!(fan.answered[0].1, 1, "one hit expected from the fixture");

    let (merged, degraded) = merge_hits(vec![], fan);
    assert!(degraded.is_none(), "no peers failed, so no degradation");
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged[0].0, peer_ident.device_id,
        "hit must be tagged with the peer machine"
    );
    assert!(merged[0]
        .1
        .excerpt
        .to_lowercase()
        .contains("purple unicorn"));
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_peer_is_reported_not_silently_dropped() {
    // A peer on an unused port: connect refused must appear in `failed`.
    let peers = vec![PeerAddr {
        device_id: "dead-peer".into(),
        name: "dead".into(),
        addr: "127.0.0.1:1".into(), // port 1: nothing listens
        // Present so this exercises the connect failure, not the missing-key
        // one.
        pubkey: Some([7u8; 32].to_vec()),
    }];
    let client_id = Identity::generate();
    let fan = query_peers(
        &client_id,
        &peers,
        "anything",
        3_600_000,
        10,
        &Default::default(),
    )
    .await
    .unwrap();
    assert!(fan.hits.is_empty());
    assert_eq!(fan.failed.len(), 1, "dead peer must be reported");
    assert_eq!(fan.failed[0].0, "dead-peer");
    assert!(
        !fan.failed[0].1.is_empty(),
        "the failure reason must be kept, not erased"
    );
    assert!(fan.answered.is_empty());

    // merge_hits must surface the failure as degraded, not hide it.
    let (_merged, degraded) = merge_hits(vec![], fan);
    assert!(
        degraded.is_some(),
        "partial answers must degrade explicitly"
    );
    assert!(degraded.as_deref().unwrap().contains("dead-peer"));
}

/// The signed path, end to end, over a non-loopback address: a signed request
/// from a trusted peer is accepted, and an untrusted signer is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn signed_fanout_against_a_non_loopback_peer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let host = Identity::generate();
    let caller = Identity::generate();

    // Build the host's state and pre-trust the caller's key.
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&host.device_id, "TestMac").unwrap();
    tp_net::pairing::record_incoming(
        db.conn(),
        &caller.device_id,
        "caller",
        &caller.verifying,
        None,
    )
    .unwrap();
    tp_net::pairing::approve(db.conn(), &caller.device_id).unwrap();
    let state = tp_serve::state_from(tp_app::App::from_parts(
        db,
        host.clone(),
        retrieval_with_fixture(&root),
    ));

    let addr = serve(state, "0.0.0.0:0".parse().unwrap()).await.unwrap();
    let Some(lan_ip) = non_loopback_ipv4() else {
        eprintln!("no non-loopback IPv4; skipping");
        return;
    };
    let peers = vec![PeerAddr {
        device_id: host.device_id.clone(),
        name: "host".into(),
        addr: format!("{lan_ip}:{}", addr.port()),
        pubkey: Some(host.verifying.to_bytes().to_vec()),
    }];

    // A trusted, signing caller gets real results over the network.
    let fan = query_peers(
        &caller,
        &peers,
        "purple unicorn",
        3_600_000,
        10,
        &Default::default(),
    )
    .await
    .unwrap();
    assert!(
        fan.failed.is_empty(),
        "a signed request from a trusted peer must be accepted: {:?}",
        fan.failed
    );
    assert_eq!(fan.answered.len(), 1);
    assert_eq!(
        fan.answered[0].1, 1,
        "the fixture hit must come back over the signed path"
    );

    // A stranger signs with a key the host does not trust: rejected, and the
    // reason is preserved rather than erased.
    let stranger = Identity::generate();
    let fan = query_peers(
        &stranger,
        &peers,
        "purple unicorn",
        3_600_000,
        10,
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(fan.failed.len(), 1, "an untrusted signer must be rejected");
    assert!(
        fan.failed[0].1.contains("401"),
        "the 401 must be reported, got: {}",
        fan.failed[0].1
    );
    assert!(fan.hits.is_empty());
}

fn non_loopback_ipv4() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("10.255.255.255:1").ok()?;
    let ip = s.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip.to_string())
    }
}

/// The signature covers the query string, so a captured signature cannot be
/// replayed against another query; and `Content-Digest` is compared to the
/// actual body, so the body is authenticated too.
#[tokio::test(flavor = "multi_thread")]
async fn signature_covers_query_string_and_body() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let host = Identity::generate();
    let caller = Identity::generate();
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&host.device_id, "TestMac").unwrap();
    tp_net::pairing::record_incoming(
        db.conn(),
        &caller.device_id,
        "caller",
        &caller.verifying,
        None,
    )
    .unwrap();
    tp_net::pairing::approve(db.conn(), &caller.device_id).unwrap();
    let addr = serve(
        tp_serve::state_from(tp_app::App::from_parts(
            db,
            host.clone(),
            retrieval_with_fixture(&root),
        )),
        "0.0.0.0:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let Some(lan_ip) = non_loopback_ipv4() else {
        return;
    };
    let base = format!("https://{lan_ip}:{}", addr.port());
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let signed_pq = "/v1/search?q=unicorn&since_ms=3600000&limit=10";
    let signed = tp_net::sign_request(&caller, "GET", signed_pq, b"", None).unwrap();

    // The exact signed request is accepted.
    let ok = client
        .get(format!("{base}{signed_pq}"))
        .header(
            tp_net::auth::SIGNATURE_INPUT_HEADER,
            &signed.signature_input,
        )
        .header(tp_net::auth::SIGNATURE_HEADER, &signed.signature)
        .header(tp_net::auth::CONTENT_DIGEST_HEADER, &signed.content_digest)
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok.status(),
        200,
        "the exact signed request must be accepted"
    );

    // Same signature, different query: rejected.
    let tampered = client
        .get(format!(
            "{base}/v1/search?q=password&since_ms=99999999999&limit=100000"
        ))
        .header(
            tp_net::auth::SIGNATURE_INPUT_HEADER,
            &signed.signature_input,
        )
        .header(tp_net::auth::SIGNATURE_HEADER, &signed.signature)
        .header(tp_net::auth::CONTENT_DIGEST_HEADER, &signed.content_digest)
        .send()
        .await
        .unwrap();
    assert_eq!(
        tampered.status(),
        401,
        "a signature must not transfer to a different query"
    );

    // Content-Digest that does not match the actual body: rejected. A GET
    // with a mismatched digest rather than a write route, because writes are
    // refused before body verification runs; this must reach the body check.
    let mismatched =
        tp_net::sign_request(&caller, "GET", signed_pq, b"not-the-real-empty-body", None).unwrap();
    let lying = client
        .get(format!("{base}{signed_pq}"))
        .header(
            tp_net::auth::SIGNATURE_INPUT_HEADER,
            &mismatched.signature_input,
        )
        .header(tp_net::auth::SIGNATURE_HEADER, &mismatched.signature)
        .header(
            tp_net::auth::CONTENT_DIGEST_HEADER,
            &mismatched.content_digest,
        ) // digest of the WRONG body
        // actual request body sent is empty, so it won't match `mismatched.content_digest`
        .send()
        .await
        .unwrap();
    assert_eq!(
        lying.status(),
        401,
        "a body that does not match Content-Digest must be rejected"
    );
}

/// Pairing approval is never a network decision: a valid signature from a
/// trusted peer must not be able to approve arbitrary new peers into the
/// trust store.
#[tokio::test(flavor = "multi_thread")]
async fn pair_respond_is_loopback_only() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let host = Identity::generate();
    let caller = Identity::generate();
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&host.device_id, "TestMac").unwrap();
    // Trusted on purpose: even a legitimately trusted peer is denied.
    tp_net::pairing::record_incoming(
        db.conn(),
        &caller.device_id,
        "caller",
        &caller.verifying,
        None,
    )
    .unwrap();
    tp_net::pairing::approve(db.conn(), &caller.device_id).unwrap();
    let addr = serve(
        tp_serve::state_from(tp_app::App::from_parts(
            db,
            host.clone(),
            retrieval_with_fixture(&root),
        )),
        "0.0.0.0:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let Some(lan_ip) = non_loopback_ipv4() else {
        return;
    };
    let base = format!("https://{lan_ip}:{}", addr.port());
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let body = r#"{"device_id":"someone-else","accept":true}"#;
    let signed = tp_net::sign_request(
        &caller,
        "POST",
        "/v1/pair/respond",
        body.as_bytes(),
        Some("any-nonce"),
    )
    .unwrap();

    let res = client
        .post(format!("{base}/v1/pair/respond"))
        .header(
            tp_net::auth::SIGNATURE_INPUT_HEADER,
            &signed.signature_input,
        )
        .header(tp_net::auth::SIGNATURE_HEADER, &signed.signature)
        .header(tp_net::auth::CONTENT_DIGEST_HEADER, &signed.content_digest)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        403,
        "a trusted peer's valid signature must still be rejected on this route"
    );
}

/// A peer whose response does not verify contributes nothing, and says so. If
/// a mis-signed response were a soft failure whose hits still flowed through,
/// an attacker would simply never sign. There is no flag to relax it: the hits
/// land in a coding agent's context, so an unverified hit is a
/// prompt-injection channel, not a wrong search result.
#[tokio::test(flavor = "multi_thread")]
async fn a_response_signed_by_the_wrong_key_is_dropped_not_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let peer_ident = Identity::generate();
    // The caller must be trusted, or the request is refused before a response
    // exists and this asserts a 401 instead of the response-signature check.
    let client_id = Identity::generate();
    let addr = serve(
        make_state_trusting(peer_ident.clone(), &root, &client_id),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    // The peer is genuine and answers, but we hold the wrong key for it —
    // indistinguishable from an impostor answering in its place.
    let impostor = Identity::generate();
    let peers = vec![PeerAddr {
        device_id: peer_ident.device_id.clone(),
        name: "peer".into(),
        addr: format!("127.0.0.1:{}", addr.port()),
        pubkey: Some(impostor.verifying.to_bytes().to_vec()),
    }];

    let fan = query_peers(
        &client_id,
        &peers,
        "purple unicorn",
        3_600_000,
        10,
        &Default::default(),
    )
    .await
    .unwrap();

    assert!(
        fan.hits.is_empty(),
        "hits from an unverifiable response must not reach the caller, got {:?}",
        fan.hits
    );
    assert_eq!(
        fan.failed.len(),
        1,
        "and the peer must be REPORTED as failed — silently returning zero hits \
         reads as 'nothing was discussed there', which is the false negative \
         this whole module is written to avoid"
    );
    assert!(
        fan.failed[0].1.contains("signature"),
        "the reason must name the actual cause, got {:?}",
        fan.failed[0].1
    );
}

/// A trusted peer with no stored public key is unverifiable, so it is dropped
/// the same way — before a request is even sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_with_no_stored_key_is_reported_not_queried() {
    let peers = vec![PeerAddr {
        device_id: "keyless".into(),
        name: "keyless".into(),
        addr: "127.0.0.1:1".into(),
        pubkey: None,
    }];
    let client_id = Identity::generate();
    let fan = query_peers(
        &client_id,
        &peers,
        "anything",
        3_600_000,
        10,
        &Default::default(),
    )
    .await
    .unwrap();
    assert!(fan.hits.is_empty());
    assert_eq!(fan.failed.len(), 1);
    assert!(
        fan.failed[0].1.contains("public key"),
        "got {:?}",
        fan.failed[0].1
    );
}

/// Trust is checked fresh on every request, never cached from when it was
/// granted: the same signed request from the same key is accepted before a
/// revoke and refused after, with no restart and nothing changed on the
/// peer's side. The only thing that moved is one row's `trust` column.
#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_trusted_peer_cuts_it_off_on_its_very_next_request() {
    let host = Identity::generate();
    let peer = Identity::generate();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("fixture");

    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&host.device_id, "TestMac").unwrap();
    tp_net::pairing::request_out(db.conn(), &peer.device_id, "peer", &peer.verifying, None)
        .unwrap();
    tp_net::pairing::approve(db.conn(), &peer.device_id).unwrap();
    let state = state_from(tp_app::App::from_parts(
        db,
        host.clone(),
        retrieval_with_fixture(&root),
    ));

    let addr = serve(state.clone(), "0.0.0.0:0".parse().unwrap())
        .await
        .unwrap();
    let Some(lan_ip) = non_loopback_ipv4() else {
        eprintln!("no non-loopback IPv4; skipping");
        return;
    };
    let peers = vec![PeerAddr {
        device_id: host.device_id.clone(),
        name: "host".into(),
        addr: format!("{lan_ip}:{}", addr.port()),
        pubkey: Some(host.verifying.to_bytes().to_vec()),
    }];

    // Before: a trusted signer gets real results.
    let before = query_peers(
        &peer,
        &peers,
        "purple unicorn",
        3_600_000,
        10,
        &Default::default(),
    )
    .await
    .unwrap();
    assert!(
        before.failed.is_empty(),
        "must be accepted while still trusted: {:?}",
        before.failed
    );
    assert_eq!(before.answered.len(), 1);

    // The revoke is a local write to the host's database; the peer sees
    // nothing happen.
    {
        let app = state.app.lock().unwrap();
        app.pair_revoke(&peer.device_id).unwrap();
        assert!(
            app.machine(&peer.device_id).unwrap().is_none(),
            "revoke removes the relationship outright — there is no negative state"
        );
    }

    // After: the identical signer, refused. `lookup_trusted_pubkey` re-reads
    // the row and finds no trusted key to verify any signature against.
    let after = query_peers(
        &peer,
        &peers,
        "purple unicorn",
        3_600_000,
        10,
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        after.failed.len(),
        1,
        "a revoked peer's signed request must now be refused"
    );
    assert!(
        after.failed[0].1.contains("401"),
        "got: {}",
        after.failed[0].1
    );
    assert!(after.hits.is_empty(), "no data may reach a revoked peer");
}

/// A peer that cannot narrow the way it was asked must say so. An older peer
/// ignores an unknown query parameter silently and nothing in the request can
/// reveal that, so the response carries `applied` and its absence is the
/// signal. Asserted against the real server rather than a stub, because the
/// claim is about what the server does with the parameters.
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_applies_the_narrowing_is_not_reported_as_degraded() {
    // Driven through `query_peers`, not a raw GET, because the signature and
    // the trust check are part of the path being asserted: an unsigned
    // request 401s, and a test that parsed that body would assert nothing.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    // Two identities: a machine cannot pair with itself, so the host cannot
    // stand in as its own caller.
    let peer_ident = Identity::generate();
    let client_id = Identity::generate();
    let addr = serve(
        make_state_trusting(peer_ident.clone(), &root, &client_id),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let peers = vec![PeerAddr {
        device_id: peer_ident.device_id.clone(),
        name: "peer".into(),
        addr: format!("127.0.0.1:{}", addr.port()),
        pubkey: Some(peer_ident.verifying.to_bytes().to_vec()),
    }];

    let narrow = tp_net::PeerQuery {
        regex: true,
        folder: Some("/Users/test/dev/demo".into()),
        until_ms: Some(tp_core::now_ms().get()),
        ..Default::default()
    };
    let fan = query_peers(&client_id, &peers, "purple", 3_600_000, 10, &narrow)
        .await
        .unwrap();
    assert!(
        fan.failed.is_empty(),
        "the peer must answer: {:?}",
        fan.failed
    );

    let (_, degraded) = merge_hits(vec![], fan);
    assert!(
        degraded.is_none(),
        "this peer applies every filter it was sent, so nothing is degraded: {degraded:?}"
    );

    // A second filter through the same path: `runtimes` is on the wire too,
    // so it must also be acknowledged.
    let unknown = tp_net::PeerQuery {
        runtimes: vec!["no_such_runtime".into()],
        ..Default::default()
    };
    let fan2 = query_peers(&client_id, &peers, "purple", 3_600_000, 10, &unknown)
        .await
        .unwrap();
    assert!(fan2.failed.is_empty());
    let (_, d2) = merge_hits(vec![], fan2);
    assert!(
        d2.is_none(),
        "runtimes IS on this build's wire, so it must be acknowledged too: {d2:?}"
    );
}
