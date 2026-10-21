//! The stdio shim Claude clients launch. It never parses MCP: it finds or
//! starts the agent and copies bytes, so any number of clients share the one
//! process that holds the Signal connection.

use relay_core::ProfileName;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Serves MCP on stdin/stdout by connecting to the profile's agent (starting
/// it if needed) and copying bytes both ways.
pub async fn run_stdio(profile: &ProfileName) -> anyhow::Result<()> {
    anyhow::bail!("stdio front end for \"{profile}\" is not implemented yet (lane L4)")
}

/// Copies bytes between a client and the agent until the agent hangs up.
// NOTE: the client closing its side only half-closes toward the agent, so
// replies already in flight still reach the client; the shim exits when the
// agent's side ends, which is also how a dead agent ends the session.
pub(crate) async fn pipe<C, A>(client: C, agent: A) -> anyhow::Result<()>
where
    C: AsyncRead + AsyncWrite,
    A: AsyncRead + AsyncWrite,
{
    let (mut client_rx, mut client_tx) = tokio::io::split(client);
    let (mut agent_rx, mut agent_tx) = tokio::io::split(agent);
    let upstream = async {
        tokio::io::copy(&mut client_rx, &mut agent_tx).await?;
        agent_tx.shutdown().await
    };
    let downstream = async {
        tokio::io::copy(&mut agent_rx, &mut client_tx).await?;
        client_tx.shutdown().await
    };
    tokio::pin!(downstream);
    tokio::select! {
        done = &mut downstream => done?,
        sent = upstream => {
            sent?;
            downstream.await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;
    use relay_agent::{Agent, endpoint};
    use relay_core::prelude::*;
    use relay_signal::{Account, FakeSignal, Outgoing};
    use relay_store::MemoryStore;
    use relay_tools::{Registry, Tool, ToolContext, ToolError};
    use rmcp::{ServiceExt, model::CallToolRequestParams};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    /// Sends its `body` to a fixed contact, so the test can count sends.
    struct ProbeSend;

    #[async_trait]
    impl Tool for ProbeSend {
        fn name(&self) -> &'static str {
            "probe_send"
        }
        fn description(&self) -> &'static str {
            "test-only write tool"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {"body": {"type": "string"}}})
        }
        fn writes(&self) -> bool {
            true
        }
        async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
            let Some(body) = input.get("body").and_then(Value::as_str) else {
                return Err(ToolError::InvalidInput("body is required".into()));
            };
            let to = Recipient::Contact(ServiceId::new(Uuid::from_u128(2)));
            let body = body.to_owned();
            ctx.signal
                .send(to, Outgoing::Text { body, quote: None })
                .await?;
            Ok(json!({}))
        }
    }

    fn test_endpoint(dir: &std::path::Path) -> String {
        if cfg!(windows) {
            format!(r"\\.\pipe\signal-agent-test-{}", Uuid::new_v4())
        } else {
            dir.join("p").join("agent.sock").display().to_string()
        }
    }

    /// One shim as Claude would run it: an MCP client on one end of a byte
    /// stream, `pipe` copying it to a fresh endpoint connection.
    async fn shim(name: &str) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        let (client_io, shim_io) = tokio::io::duplex(4096);
        let agent = endpoint::connect(name).await.expect("connect");
        tokio::spawn(pipe(shim_io, agent));
        ().serve(client_io)
            .await
            .expect("initialize through the shim")
    }

    /// PRD acceptance 8: two clients (Claude Code and Claude Desktop) driving
    /// one agent at once must not error or corrupt each other, and every send
    /// must reach Signal exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_shims_interleave_100_operations_without_loss_or_duplication() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = test_endpoint(tmp.path());
        let signal = Arc::new(FakeSignal::new(Account {
            service_id: ServiceId::new(Uuid::from_u128(1)),
            phone_number: None,
            kind: ProfileKind::Linked,
            device_name: "test".into(),
        }));
        let agent = Agent::new(
            signal.clone(),
            Arc::new(MemoryStore::default()),
            Registry::default().with(ProbeSend),
            AccessMode::Full,
        );
        let listener = endpoint::Listener::bind(&name).await.expect("bind");
        let served = tokio::spawn(agent.serve(listener));

        let code = shim(&name).await;
        let desktop = shim(&name).await;
        let calls = (0..100).map(|i| {
            let client = if i % 2 == 0 { &code } else { &desktop };
            let Value::Object(args) = json!({"body": format!("op-{i}")}) else {
                unreachable!()
            };
            let params = CallToolRequestParams::new("probe_send").with_arguments(args);
            async move { (i, client.peer().call_tool(params).await) }
        });
        let results = futures::future::join_all(calls).await;
        served.abort();

        let failures: Vec<_> = results
            .iter()
            .filter(|(_, r)| r.as_ref().map_or(true, |r| r.is_error == Some(true)))
            .collect();
        assert!(failures.is_empty(), "operations failed: {failures:?}");
        let mut counts: HashMap<String, usize> = HashMap::new();
        for (_, sent) in signal.sent() {
            let Outgoing::Text { body, .. } = sent else {
                panic!("unexpected send {sent:?}")
            };
            *counts.entry(body).or_default() += 1;
        }
        let wrong: Vec<_> = (0..100)
            .map(|i| format!("op-{i}"))
            .filter(|b| counts.get(b) != Some(&1))
            .collect();
        assert!(
            wrong.is_empty(),
            "not sent exactly once: {wrong:?} (all: {counts:?})"
        );
        assert_eq!(counts.len(), 100, "unexpected extra sends: {counts:?}");
    }
}
