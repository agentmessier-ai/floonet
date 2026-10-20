//! Exempt: test output goes to the test harness, which is the point of it.

//! What the read routes accept from a trusted peer, and what their answer
//! claims about itself: a bounded result set, a needle the caller actually
//! gave, a response signature bound to the nonce as it was sent, and an
//! `applied` list that reports what took effect rather than what arrived.

use std::net::SocketAddr;
use tp_db::Db;
use tp_net::Identity;
use tp_serve::{serve, state_from};

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp(ms / 1000, ((ms % 1000) * 1_000_000) as u32)
        .unwrap()
        .to_rfc3339()
}

/// A scan retrieval over one fixture session, so a query that is answered has
/// something to answer with and an unbounded one has a corpus to open.
fn retrieval_with_fixture(root: &std::path::Path) -> tp_search::Retrieval {
    let dir = root.join("-Users-test-dev-demo");
    std::fs::create_dir_all(&dir).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let line = serde_json::json!({
        "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": rfc3339(now),
        "message": {"content": "the purple unicorn flies at dawn"}
    });
    std::fs::write(
        dir.join("ffffffff-1111-2222-3333-444444444444.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    tp_search::Retrieval::new(Box::new(tp_search::ScanProvider::new(
        "test-machine",
        vec![Box::new(tp_ingest::builtin("claude_code"))],
        vec![("claude_code".to_string(), root.to_path_buf())],
    )))
}

/// A host that already trusts `caller`: every route under test requires a
/// signature from a trusted device, and there is no loopback exemption.
async fn serve_trusting(root: &std::path::Path, host: &Identity, caller: &Identity) -> SocketAddr {
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&host.device_id, "TestMac").unwrap();
    tp_net::pairing::request_out(
        db.conn(),
        &caller.device_id,
        "caller",
        &caller.verifying,
        None,
    )
    .unwrap();
    tp_net::pairing::approve(db.conn(), &caller.device_id).unwrap();
    let state = state_from(tp_app::App::from_parts(
        db,
        host.clone(),
        retrieval_with_fixture(root),
    ));
    serve(state, "127.0.0.1:0".parse().unwrap()).await.unwrap()
}

struct Answer {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }
}

/// `path_and_query` is signed exactly as sent — the signature covers `@query`,
/// so the string here is both the request and what was authenticated.
async fn signed_get(caller: &Identity, addr: SocketAddr, pq: &str) -> Answer {
    let signed = tp_net::sign_request(caller, "GET", pq, b"", None).unwrap();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let res = client
        .get(format!("https://127.0.0.1:{}{pq}", addr.port()))
        .header(
            tp_net::auth::SIGNATURE_INPUT_HEADER,
            &signed.signature_input,
        )
        .header(tp_net::auth::SIGNATURE_HEADER, &signed.signature)
        .header(tp_net::auth::CONTENT_DIGEST_HEADER, &signed.content_digest)
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = res.bytes().await.unwrap().to_vec();
    Answer {
        status,
        headers,
        body,
    }
}

/// A trusted peer is still a remote caller, and `limit` is the one parameter
/// that sizes the answer: unbounded, one request becomes a scan of the whole
/// corpus collected in memory. A limit this server cannot honour as written is
/// refused rather than quietly reduced — a smaller answer to a larger question
/// still reads as "this is what is there".
#[tokio::test(flavor = "multi_thread")]
async fn a_limit_this_server_will_not_honour_is_refused_not_quietly_reduced() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    let host = Identity::generate();
    let caller = Identity::generate();
    let addr = serve_trusting(&root, &host, &caller).await;

    for route in [
        "/v1/search?q=purple&since_ms=3600000&limit=100000",
        "/v1/sessions?since_ms=3600000&limit=100000",
    ] {
        let got = signed_get(&caller, addr, route).await;
        assert_eq!(
            got.status, 400,
            "{route} must refuse an unbounded result set"
        );
    }

    // A limit that is not a number is the same fault, not a silent default:
    // answering 20 to a request that asked for something else is a different
    // question answered as if it were the one asked.
    let junk = signed_get(&caller, addr, "/v1/search?q=purple&limit=all").await;
    assert_eq!(junk.status, 400, "an unparseable limit must be refused");

    // And an ordinary limit still answers, or the cap has eaten the route.
    let ok = signed_get(
        &caller,
        addr,
        "/v1/search?q=purple&since_ms=3600000&limit=10",
    )
    .await;
    assert_eq!(ok.status, 200, "a reasonable limit must still be served");
    assert_eq!(
        ok.json()["hits"].as_array().unwrap().len(),
        1,
        "the fixture hit must still come back"
    );
}

/// An absent or empty needle matches every line of every transcript, so the
/// widest read this server can perform would be the easiest request to make by
/// accident. The caller has to say what it is looking for.
#[tokio::test(flavor = "multi_thread")]
async fn a_search_with_no_needle_is_refused_rather_than_answered_with_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    let host = Identity::generate();
    let caller = Identity::generate();
    let addr = serve_trusting(&root, &host, &caller).await;

    for route in [
        "/v1/search?since_ms=3600000&limit=10",
        "/v1/search?q=&since_ms=3600000&limit=10",
    ] {
        let got = signed_get(&caller, addr, route).await;
        assert_eq!(
            got.status,
            400,
            "{route} must not return the whole corpus; body was {}",
            String::from_utf8_lossy(&got.body)
        );
    }
}

/// The response signature carries the caller's nonce, which binds this answer
/// to that question. The nonce travels in the query string, so it arrives
/// percent-encoded; signing the raw encoded text produces a nonce the caller
/// never sent, and its verification fails with nothing to point at.
#[tokio::test(flavor = "multi_thread")]
async fn a_percent_encoded_nonce_is_answered_with_the_nonce_the_caller_sent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    let host = Identity::generate();
    let caller = Identity::generate();
    let addr = serve_trusting(&root, &host, &caller).await;

    // The nonce is `a b/c`: a space and a slash, neither of which survives a
    // query string unencoded.
    let nonce = "a b/c";
    let got = signed_get(
        &caller,
        addr,
        "/v1/search?q=purple&since_ms=3600000&limit=10&nonce=a%20b%2Fc",
    )
    .await;
    assert_eq!(got.status, 200);
    assert!(
        tp_net::auth::verify_response(
            got.status,
            &got.body,
            &got.header(tp_net::auth::SIGNATURE_INPUT_HEADER),
            &got.header(tp_net::auth::SIGNATURE_HEADER),
            &got.header(tp_net::auth::CONTENT_DIGEST_HEADER),
            &host.device_id,
            nonce,
            &host.verifying,
        ),
        "the answer must be signed over the nonce as the caller sent it"
    );
}

/// A parameter whose name merely ends in `nonce` is a different parameter.
/// Reading it as the nonce signs the answer against a value the caller is not
/// expecting, which fails verification and drops a good peer's results.
#[tokio::test(flavor = "multi_thread")]
async fn a_parameter_whose_name_ends_in_nonce_is_not_read_as_the_nonce() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    let host = Identity::generate();
    let caller = Identity::generate();
    let addr = serve_trusting(&root, &host, &caller).await;

    let got = signed_get(
        &caller,
        addr,
        "/v1/search?q=purple&limit=10&my_nonce=decoy&nonce=real",
    )
    .await;
    assert_eq!(got.status, 200);
    assert!(
        tp_net::auth::verify_response(
            got.status,
            &got.body,
            &got.header(tp_net::auth::SIGNATURE_INPUT_HEADER),
            &got.header(tp_net::auth::SIGNATURE_HEADER),
            &got.header(tp_net::auth::CONTENT_DIGEST_HEADER),
            &host.device_id,
            "real",
            &host.verifying,
        ),
        "the real nonce must be the one signed over"
    );
}

/// `applied` is what the caller reads to decide whether its narrowing was
/// honoured, so it must report what took effect. A parameter that arrived but
/// did not parse changed nothing about the answer, and reporting it as applied
/// tells the caller its query was narrower than it was.
#[tokio::test(flavor = "multi_thread")]
async fn applied_lists_the_narrowings_that_took_effect_not_the_ones_that_arrived() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    let host = Identity::generate();
    let caller = Identity::generate();
    let addr = serve_trusting(&root, &host, &caller).await;

    // `regex=yes` is not a value this server reads as true, so the search ran
    // without regex. The caller must learn that, or it believes it got a
    // regex match.
    let got = signed_get(
        &caller,
        addr,
        "/v1/search?q=purple&since_ms=3600000&limit=10&regex=yes&runtimes=",
    )
    .await;
    assert_eq!(got.status, 200);
    let applied: Vec<String> = serde_json::from_value(got.json()["applied"].clone()).unwrap();
    assert!(
        !applied.contains(&"regex".to_string()),
        "regex did not take effect, so it must not be reported as applied: {applied:?}"
    );
    assert!(
        !applied.contains(&"runtimes".to_string()),
        "an empty runtimes list narrows nothing: {applied:?}"
    );

    // The other half of the contract: a narrowing that DID take effect must
    // still be reported, or the caller degrades every answer it gets.
    let got = signed_get(
        &caller,
        addr,
        "/v1/search?q=purple&since_ms=3600000&limit=10&regex=1&folder=/Users/test/dev/demo",
    )
    .await;
    assert_eq!(got.status, 200);
    let applied: Vec<String> = serde_json::from_value(got.json()["applied"].clone()).unwrap();
    assert!(
        applied.contains(&"regex".to_string()) && applied.contains(&"folder".to_string()),
        "a narrowing that took effect must be acknowledged: {applied:?}"
    );
}
