//! The one process per profile that holds the Signal connection. Everything
//! else (stdio shims, the HTTP front end, setup) reaches Signal through it,
//! because two processes driving one Signal device corrupt its sessions.

mod capture;
pub mod endpoint;
pub mod login;
mod mcp;
mod startup;

use std::sync::Arc;

use anyhow::bail;
use relay_core::prelude::*;
use relay_signal::SignalBackend;
use relay_store::MessageStore;
use relay_tools::{Registry, ToolContext};
use rmcp::ServiceExt;

/// What the agent needs to start.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub profile: ProfileName,
    pub paths: ProfilePaths,
    pub access: AccessMode,
}

/// A profile's agent around an already-open Signal backend and message store.
pub struct Agent {
    signal: Arc<dyn SignalBackend>,
    store: Arc<dyn MessageStore>,
    registry: Arc<Registry>,
    access: AccessMode,
}

impl Agent {
    pub fn new(
        signal: Arc<dyn SignalBackend>,
        store: Arc<dyn MessageStore>,
        registry: Registry,
        access: AccessMode,
    ) -> Self {
        Self {
            signal,
            store,
            registry: Arc::new(registry),
            access,
        }
    }

    /// Captures everything Signal delivers into the store and serves MCP to
    /// every connection on `listener`, until accepting fails.
    pub async fn serve(self, listener: endpoint::Listener) -> anyhow::Result<()> {
        let kind = self.signal.account().await?.kind;
        let handler = mcp::Handler {
            ctx: ToolContext {
                store: self.store.clone(),
                signal: self.signal.clone(),
                access: self.access,
                kind,
            },
            registry: self.registry,
        };
        let capture = tokio::spawn(capture::run(self.signal, self.store));
        let result = accept_loop(listener, handler).await;
        capture.abort();
        result
    }
}

async fn accept_loop(
    mut listener: endpoint::Listener,
    handler: mcp::Handler,
) -> anyhow::Result<()> {
    loop {
        let stream = listener.accept().await?;
        let handler = handler.clone();
        tokio::spawn(async move {
            let session = match handler.serve(stream).await {
                Ok(session) => session,
                Err(e) => return tracing::warn!("client failed to initialize: {e}"),
            };
            if let Err(e) = session.waiting().await {
                tracing::warn!("client session ended with an error: {e}");
            }
        });
    }
}

/// Runs the agent until it is told to stop: receives continuously into the
/// store and serves MCP JSON-RPC on the profile's user-only local endpoint.
pub async fn run(config: AgentConfig) -> anyhow::Result<()> {
    // NOTE: bind before opening Signal. The bind is the single-instance check;
    // opening presage's store first would let a second agent touch the same
    // device state before discovering it should exit.
    let name = endpoint::name(&config.profile, &config.paths)?;
    let listener = endpoint::Listener::bind(&name).await?;
    let profile = config.profile.clone();
    // Off the critical path: it shells out to Claude's CLIs, and the agent must
    // start capturing whether or not that works.
    tokio::task::spawn_blocking(move || startup::reregister_claude_entries(&profile));
    let (signal, store) = open_backend_and_store(&config).await?;
    Agent::new(signal, store, Registry::standard(), config.access)
        .serve(listener)
        .await
}

// Integration seam: the presage backend (lane L1) and the SQLCipher store with
// its key from relay-secrets (lane L2) are opened here once they exist. Tests
// never come through this path; they build an `Agent` around fakes.
async fn open_backend_and_store(
    config: &AgentConfig,
) -> anyhow::Result<(Arc<dyn SignalBackend>, Arc<dyn MessageStore>)> {
    bail!(
        "profile \"{}\": the presage backend and SQLCipher store are not wired into the agent yet",
        config.profile
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use relay_signal::{Account, FakeSignal, Incoming};
    use relay_store::{MemoryStore, SearchQuery};
    use rmcp::model::CallToolRequestParams;
    use uuid::Uuid;

    use super::*;
    use crate::mcp::tests::ProbeSend;

    /// A fresh endpoint name no other test uses.
    pub(crate) fn test_endpoint(dir: &std::path::Path) -> String {
        if cfg!(windows) {
            format!(r"\\.\pipe\signal-agent-test-{}", Uuid::new_v4())
        } else {
            dir.join("p").join("agent.sock").display().to_string()
        }
    }

    pub(crate) fn fake_signal() -> Arc<FakeSignal> {
        Arc::new(FakeSignal::new(Account {
            service_id: ServiceId::new(Uuid::from_u128(1)),
            phone_number: None,
            kind: ProfileKind::Linked,
            device_name: "test".into(),
        }))
    }

    /// The agent must capture and serve at once: a client's send reaches Signal
    /// over the endpoint while an incoming message lands in the store.
    #[tokio::test]
    async fn the_agent_serves_mcp_on_its_endpoint_while_capturing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = test_endpoint(tmp.path());
        let signal = fake_signal();
        let store = Arc::new(MemoryStore::default());
        let agent = Agent::new(
            signal.clone(),
            store.clone(),
            Registry::default().with(ProbeSend),
            AccessMode::Full,
        );
        let listener = endpoint::Listener::bind(&name).await.expect("bind");
        let served = tokio::spawn(agent.serve(listener));

        let stream = endpoint::connect(&name).await.expect("connect");
        let client = ().serve(stream).await.expect("initialize");
        let args = serde_json::json!({"body": "hi"});
        let serde_json::Value::Object(args) = args else {
            unreachable!()
        };
        let result = client
            .peer()
            .call_tool(CallToolRequestParams::new("probe_send").with_arguments(args))
            .await
            .expect("tools/call");
        assert_ne!(result.is_error, Some(true), "send failed: {result:?}");
        assert_eq!(signal.sent().len(), 1, "send did not reach Signal");

        let sender = ServiceId::new(Uuid::from_u128(2));
        signal.push(Incoming::Message(Message {
            id: MessageId {
                author: sender,
                sent_at_ms: 10,
            },
            conversation: Recipient::Contact(sender),
            sender,
            from_me: false,
            received_at_ms: 10,
            body: Some("captured".into()),
            attachments: vec![],
            quote: None,
            edited: false,
            deleted: false,
        }));
        let query = SearchQuery {
            text: "captured".into(),
            limit: 1,
            ..Default::default()
        };
        let mut found = vec![];
        for _ in 0..200 {
            found = store.search(&query).await.expect("search");
            if !found.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        served.abort();
        assert_eq!(
            found.len(),
            1,
            "incoming message not captured while serving"
        );
    }
}
