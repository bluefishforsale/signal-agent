//! MCP JSON-RPC as the agent serves it: tools/list and tools/call straight onto
//! the registry, so the access rules there are the only ones that exist.

use std::sync::Arc;

use relay_core::AccessMode;
use relay_tools::{Registry, ToolContext, ToolError};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
};
use serde_json::Value;

/// The experimental client capability a front end declares at initialize to
/// have its connection served read-only, whatever the agent's own mode.
pub const READ_ONLY_CAPABILITY: &str = "signal-agent/read-only";

/// One client connection's view of the agent.
#[derive(Clone)]
pub(crate) struct Handler {
    pub(crate) ctx: ToolContext,
    pub(crate) registry: Arc<Registry>,
}

impl Handler {
    /// The context this client's calls run in: the agent's, made read-only
    /// when the client asked for that. A client can only lower its access.
    fn ctx_for(&self, context: &RequestContext<RoleServer>) -> ToolContext {
        let read_only = context.peer.peer_info().is_some_and(|info| {
            info.capabilities
                .experimental
                .as_ref()
                .is_some_and(|e| e.contains_key(READ_ONLY_CAPABILITY))
        });
        let mut ctx = self.ctx.clone();
        if read_only {
            ctx.access = AccessMode::ReadOnly;
        }
        ctx
    }
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let ctx = self.ctx_for(&context);
        let tools = self
            .registry
            .visible(&ctx)
            .map(|tool| {
                let Value::Object(schema) = tool.input_schema() else {
                    return Err(ErrorData::internal_error(
                        format!("tool \"{}\" has a non-object input schema", tool.name()),
                        None,
                    ));
                };
                Ok(Tool::new(tool.name(), tool.description(), schema))
            })
            .collect::<Result<_, _>>()?;
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let input = Value::Object(request.arguments.unwrap_or_default());
        let ctx = self.ctx_for(&context);
        let output = self.registry.call(&ctx, &request.name, input).await;
        // A malformed request is a protocol error the client fixes; a refusal or a
        // failure inside the tool is a result the model reads and explains.
        match output {
            Ok(value) => Ok(CallToolResult::structured(value).into()),
            Err(e @ (ToolError::Unknown(_) | ToolError::InvalidInput(_))) => {
                Err(ErrorData::invalid_params(e.to_string(), None))
            }
            Err(e @ (ToolError::ReadOnly(_) | ToolError::NeedsPrimary(_))) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())]).into())
            }
            Err(ToolError::Internal(e)) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]).into())
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use async_trait::async_trait;
    use relay_core::prelude::*;
    use relay_signal::{Account, FakeSignal, Outgoing};
    use relay_store::MemoryStore;
    use relay_tools::Tool as RelayTool;
    use rmcp::{ServiceExt, model::ErrorCode};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    /// A write tool that sends its `body` to a fixed contact, so tests can
    /// count exactly what reached Signal.
    pub(crate) struct ProbeSend;

    #[async_trait]
    impl RelayTool for ProbeSend {
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
            let id = ctx
                .signal
                .send(
                    to,
                    Outgoing::Text {
                        body: body.to_owned(),
                        quote: None,
                        attachments: vec![],
                    },
                )
                .await?;
            Ok(json!({"sent_at_ms": id.sent_at_ms}))
        }
    }

    pub(crate) fn handler(access: AccessMode) -> (Handler, Arc<FakeSignal>) {
        let signal = Arc::new(FakeSignal::new(Account {
            service_id: ServiceId::new(Uuid::from_u128(1)),
            phone_number: None,
            kind: ProfileKind::Linked,
            device_name: "test".into(),
        }));
        let ctx = ToolContext {
            store: Arc::new(MemoryStore::default()),
            signal: signal.clone(),
            access,
            kind: ProfileKind::Linked,
            send_roots: Arc::default(),
            webhook: None,
        };
        let registry = Arc::new(Registry::default().with(ProbeSend));
        (Handler { ctx, registry }, signal)
    }

    async fn connect(server: Handler) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        let (server_io, client_io) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let running = server.serve(server_io).await.expect("server starts");
            running.waiting().await
        });
        ().serve(client_io).await.expect("client connects")
    }

    fn call(name: &'static str, args: Value) -> CallToolRequestParams {
        let Value::Object(args) = args else {
            panic!("test arguments must be an object")
        };
        CallToolRequestParams::new(name).with_arguments(args)
    }

    /// A tools/call must run the tool through the registry and hand its JSON
    /// result back to the client.
    #[tokio::test]
    async fn tools_call_runs_the_tool_and_returns_its_result() {
        let (server, signal) = handler(AccessMode::Full);
        let client = connect(server).await;
        let result = client
            .peer()
            .call_tool(call("probe_send", json!({"body": "hi"})))
            .await
            .expect("tools/call");
        assert_ne!(result.is_error, Some(true), "call failed: {result:?}");
        let sent_at = result
            .structured_content
            .as_ref()
            .and_then(|v| v.get("sent_at_ms"));
        assert!(sent_at.is_some(), "tool result not returned: {result:?}");
        assert_eq!(signal.sent().len(), 1, "send did not reach Signal");
    }

    /// A client must be able to tell "you asked wrong" (a JSON-RPC invalid-params
    /// error) from "the tool refused or failed" (an isError result the model
    /// reads), and a refused write must never reach Signal.
    #[tokio::test]
    async fn tool_errors_map_to_protocol_errors_or_error_results() {
        let (server, signal) = handler(AccessMode::ReadOnly);
        let client = connect(server).await;
        let unknown = client
            .peer()
            .call_tool(call("no_such_tool", json!({})))
            .await;
        assert!(
            matches!(&unknown, Err(rmcp::ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS),
            "unknown tool: {unknown:?}"
        );

        let (server, signal_full) = handler(AccessMode::Full);
        let full = connect(server).await;
        let invalid = full.peer().call_tool(call("probe_send", json!({}))).await;
        assert!(
            matches!(&invalid, Err(rmcp::ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS),
            "invalid input: {invalid:?}"
        );
        assert!(signal_full.sent().is_empty(), "invalid call reached Signal");

        let refused = client
            .peer()
            .call_tool(call("probe_send", json!({"body": "hi"})))
            .await
            .expect("a refusal is a result, not a protocol error");
        assert_eq!(refused.is_error, Some(true), "read-only write: {refused:?}");
        assert!(signal.sent().is_empty(), "read-only write reached Signal");
    }

    /// A front end serving a read-only client (an HTTP read-only token) asks
    /// for read-only at initialize, and the registry must then hide and refuse
    /// every write tool even though the agent itself runs with full access.
    #[tokio::test]
    async fn a_client_that_declares_read_only_is_served_read_only() {
        let (server, signal) = handler(AccessMode::Full);
        let (server_io, client_io) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let running = server.serve(server_io).await.expect("server starts");
            running.waiting().await
        });
        let client = read_only_client_config()
            .serve(client_io)
            .await
            .expect("client connects");
        let tools = client.peer().list_tools(None).await.expect("tools/list");
        assert!(tools.tools.is_empty(), "write tool listed: {tools:?}");
        let refused = client
            .peer()
            .call_tool(call("probe_send", json!({"body": "hi"})))
            .await
            .expect("a refusal is a result, not a protocol error");
        assert_eq!(refused.is_error, Some(true), "read-only write: {refused:?}");
        assert!(signal.sent().is_empty(), "read-only write reached Signal");
    }

    fn read_only_client_config() -> rmcp::model::ClientConfig {
        let mut experimental = rmcp::model::ExperimentalCapabilities::new();
        experimental.insert(READ_ONLY_CAPABILITY.to_owned(), Default::default());
        let mut info = rmcp::model::ClientConfig::default();
        info.capabilities.experimental = Some(experimental);
        info
    }

    /// tools/list must be the registry's visible set: a client must see the
    /// tools it may call and nothing the registry hides.
    #[tokio::test]
    async fn tools_list_is_the_registrys_visible_set() {
        for (access, expected) in [
            (AccessMode::Full, vec!["probe_send"]),
            (AccessMode::ReadOnly, vec![]),
        ] {
            let (server, _) = handler(access);
            let (server_io, client_io) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let running = server.serve(server_io).await.expect("server starts");
                running.waiting().await
            });
            let client = ().serve(client_io).await.expect("client connects");
            let tools = client.peer().list_tools(None).await.expect("tools/list");
            let names: Vec<_> = tools.tools.iter().map(|t| t.name.to_string()).collect();
            assert_eq!(names, expected, "wrong tools listed in {access:?} mode");
        }
    }
}
