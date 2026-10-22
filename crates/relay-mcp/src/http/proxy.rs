//! Each HTTP request's MCP call, replayed on a fresh connection to the agent.
//! The agent alone runs tools, so its registry is the one gate; a read-only
//! token's calls arrive there on a connection that asked to be read-only.

use relay_agent::{READ_ONLY_CAPABILITY, endpoint};
use relay_core::AccessMode;
use rmcp::{
    ErrorData, RoleClient, RoleServer, ServerHandler, ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, ClientConfig, ExperimentalCapabilities,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    },
    service::{RequestContext, RunningService},
};

/// Forwards tools/list and tools/call to the agent with a token's scope.
#[derive(Clone)]
pub(crate) struct Proxy {
    agent: String,
    scope: AccessMode,
}

impl Proxy {
    pub(crate) fn new(agent: String, scope: AccessMode) -> Self {
        Self { agent, scope }
    }

    async fn connect(&self) -> Result<RunningService<RoleClient, ClientConfig>, ErrorData> {
        let stream = endpoint::connect(&self.agent).await.map_err(|e| {
            ErrorData::internal_error(format!("the agent is not running: {e:#}"), None)
        })?;
        let mut config = ClientConfig::default();
        if self.scope == AccessMode::ReadOnly {
            let mut experimental = ExperimentalCapabilities::new();
            experimental.insert(READ_ONLY_CAPABILITY.to_owned(), Default::default());
            config.capabilities.experimental = Some(experimental);
        }
        config.serve(stream).await.map_err(|e| {
            ErrorData::internal_error(format!("the agent refused to connect: {e}"), None)
        })
    }
}

fn upstream(e: ServiceError) -> ErrorData {
    match e {
        ServiceError::McpError(e) => e,
        e => ErrorData::internal_error(format!("the agent connection failed: {e}"), None),
    }
}

impl ServerHandler for Proxy {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let agent = self.connect().await?;
        agent.peer().list_tools(request).await.map_err(upstream)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let agent = self.connect().await?;
        let result = agent.peer().call_tool(request).await.map_err(upstream)?;
        Ok(result.into())
    }
}
