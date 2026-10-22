//! The HTTP endpoint's request log, in its own test binary because it installs
//! a global subscriber: tracing caches per-callsite interest process-wide, so a
//! thread-local subscriber in a shared test binary misses events at random.

use std::sync::{Arc, Mutex};

use relay_agent::Agent;
use relay_core::prelude::*;
use relay_signal::{Account, FakeSignal};
use relay_store::MemoryStore;
use relay_tools::Registry;
use uuid::Uuid;

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Each request must be traceable to the client that made it, by the token's
/// name, and the log must never become a place to steal a token.
#[tokio::test]
async fn every_request_is_logged_with_its_token_name_and_never_the_token() {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .init();
    let dir = tempfile::tempdir().expect("tempdir");
    let agent = if cfg!(windows) {
        format!(r"\\.\pipe\signal-agent-test-{}", Uuid::new_v4())
    } else {
        dir.path().join("agent.sock").display().to_string()
    };
    let signal = Arc::new(FakeSignal::new(Account {
        service_id: ServiceId::new(Uuid::from_u128(1)),
        phone_number: None,
        kind: ProfileKind::Primary,
        device_name: "test".into(),
    }));
    let listener = relay_agent::endpoint::Listener::bind(&agent)
        .await
        .expect("bind agent");
    let served = Agent::new(
        signal,
        Arc::new(MemoryStore::default()),
        Registry::default(),
        AccessMode::Full,
    )
    .serve(listener, std::future::pending());
    tokio::spawn(served);
    let tokens = relay_mcp::Tokens::at(&dir.path().join("http_tokens.json"));
    let token = tokens.create("laptop", AccessMode::Full).expect("create");
    let http = relay_mcp::Listener::bind(([127, 0, 0, 1], 0).into(), None)
        .await
        .expect("bind http");
    let addr = http.local_addr().expect("addr");
    tokio::spawn(relay_mcp::serve(http, tokens, agent));

    let _ = rustls::crypto::ring::default_provider().install_default();
    let status = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
        .post(format!("http://{addr}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(&token)
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .send()
        .await
        .expect("request")
        .status();
    assert_eq!(status, reqwest::StatusCode::OK);
    let logged = String::from_utf8(logs.0.lock().expect("log lock").clone()).expect("utf8");
    assert!(
        logged.contains("token=\"laptop\"") || logged.contains("token=laptop"),
        "request not logged by name: {logged}"
    );
    assert!(!logged.contains(&token), "token logged: {logged}");
}
