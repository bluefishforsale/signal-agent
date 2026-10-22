//! The HTTP listener and the gate in front of it. A request gets nowhere
//! without a live token, and the token's scope picks which agent connection
//! serves it, so a read-only token meets the registry's read-only gate.

use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use anyhow::Context;
use bytes::Bytes;
use http::{Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use relay_core::AccessMode;
use rmcp::transport::{
    StreamableHttpServerConfig, StreamableHttpService,
    streamable_http_server::session::never::NeverSessionManager,
};
use tokio::net::TcpListener;

use super::{Tokens, proxy::Proxy};

/// A bound HTTP endpoint, not yet serving.
pub struct Listener {
    tcp: TcpListener,
}

impl Listener {
    pub async fn bind(addr: SocketAddr) -> anyhow::Result<Self> {
        let tcp = TcpListener::bind(addr)
            .await
            .with_context(|| format!("unable to listen on {addr}"))?;
        Ok(Self { tcp })
    }

    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.tcp.local_addr()?)
    }
}

type Body = BoxBody<Bytes, Infallible>;

struct Gate {
    tokens: Tokens,
    full: StreamableHttpService<Proxy, NeverSessionManager>,
    read_only: StreamableHttpService<Proxy, NeverSessionManager>,
}

/// Serves MCP on `listener` for every request carrying a live token, through
/// the agent at `agent`, until accepting fails.
pub async fn serve(listener: Listener, tokens: Tokens, agent: String) -> anyhow::Result<()> {
    let gate = Arc::new(Gate {
        tokens,
        full: mcp_service(Proxy::new(agent.clone(), AccessMode::Full)),
        read_only: mcp_service(Proxy::new(agent, AccessMode::ReadOnly)),
    });
    loop {
        let (tcp, _) = listener.tcp.accept().await?;
        let gate = gate.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| {
                let gate = gate.clone();
                async move { Ok::<_, Infallible>(gate.handle(request).await) }
            });
            let served = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(tcp), service)
                .await;
            if let Err(e) = served {
                tracing::debug!("http connection ended: {e}");
            }
        });
    }
}

fn mcp_service(proxy: Proxy) -> StreamableHttpService<Proxy, NeverSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    StreamableHttpService::new(
        move || Ok(proxy.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

impl Gate {
    async fn handle(&self, request: Request<Incoming>) -> Response<Body> {
        let presented = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let grant = match presented.map(|t| self.tokens.verify(t)) {
            Some(Ok(Some(grant))) => grant,
            Some(Err(e)) => {
                tracing::error!("unable to check a token: {e:#}");
                return unauthorized();
            }
            Some(Ok(None)) | None => return unauthorized(),
        };
        let service = match grant.scope {
            AccessMode::Full => &self.full,
            AccessMode::ReadOnly => &self.read_only,
        };
        service.handle(request).await
    }
}

fn unauthorized() -> Response<Body> {
    let mut response =
        Response::new(Full::new(Bytes::from_static(b"a bearer token is required\n")).boxed());
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Bearer"),
    );
    response
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use relay_agent::Agent;
    use relay_core::prelude::*;
    use relay_signal::{Account, FakeSignal, Outgoing};
    use relay_store::MemoryStore;
    use relay_tools::{Registry, Tool, ToolContext, ToolError};
    use rmcp::{
        RoleClient, ServiceExt,
        model::CallToolRequestParams,
        service::RunningService,
        transport::{
            StreamableHttpClientTransport,
            streamable_http_client::StreamableHttpClientTransportConfig,
        },
    };
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    /// Sends its `body` to a fixed contact, so tests can see what reached Signal.
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
            let outgoing = Outgoing::Text {
                body: body.to_owned(),
                quote: None,
                attachments: vec![],
            };
            ctx.signal.send(to, outgoing).await?;
            Ok(json!({}))
        }
    }

    fn fake_signal() -> Arc<FakeSignal> {
        Arc::new(FakeSignal::new(Account {
            service_id: ServiceId::new(Uuid::from_u128(1)),
            phone_number: None,
            kind: ProfileKind::Primary,
            device_name: "test".into(),
        }))
    }

    /// A full-access agent serving `registry` on a fresh local endpoint.
    async fn agent(dir: &std::path::Path, registry: Registry) -> (String, Arc<FakeSignal>) {
        let name = if cfg!(windows) {
            format!(r"\\.\pipe\signal-agent-test-{}", Uuid::new_v4())
        } else {
            dir.join("agent.sock").display().to_string()
        };
        let signal = fake_signal();
        let agent = Agent::new(
            signal.clone(),
            Arc::new(MemoryStore::default()),
            registry,
            AccessMode::Full,
        );
        let listener = relay_agent::endpoint::Listener::bind(&name)
            .await
            .expect("bind agent");
        tokio::spawn(agent.serve(listener, std::future::pending()));
        (name, signal)
    }

    /// An MCP client as Claude Code runs it with `--transport http` and a
    /// bearer header.
    async fn remote(addr: SocketAddr, token: &str) -> RunningService<RoleClient, ()> {
        let config = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(token);
        let transport = StreamableHttpClientTransport::with_client(client(), config);
        ().serve(transport).await.expect("initialize over http")
    }

    fn call(name: &'static str, args: Value) -> CallToolRequestParams {
        let Value::Object(args) = args else {
            panic!("test arguments must be an object")
        };
        CallToolRequestParams::new(name).with_arguments(args)
    }

    /// A full-scope token must reach the agent's tools end to end: listed,
    /// callable, and a send lands in Signal exactly once.
    #[tokio::test]
    async fn a_full_token_lists_and_calls_tools_through_the_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (agent, signal) = agent(dir.path(), Registry::default().with(ProbeSend)).await;
        let tokens = Tokens::at(&dir.path().join("http_tokens.json"));
        let token = tokens.create("laptop", AccessMode::Full).expect("create");
        let addr = start(tokens, agent).await;
        let client = remote(addr, &token).await;
        let tools = client.peer().list_tools(None).await.expect("tools/list");
        let names: Vec<_> = tools.tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["probe_send"]);
        let result = client
            .peer()
            .call_tool(call("probe_send", json!({"body": "hi"})))
            .await
            .expect("tools/call");
        assert_ne!(result.is_error, Some(true), "call failed: {result:?}");
        assert_eq!(signal.sent().len(), 1, "send did not reach Signal once");
    }

    async fn start(tokens: Tokens, agent: String) -> SocketAddr {
        let listener = Listener::bind(([127, 0, 0, 1], 0).into())
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(serve(listener, tokens, agent));
        addr
    }

    fn client() -> reqwest::Client {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client")
    }

    /// PRD acceptance 10: a request with no token must be refused before it
    /// reaches the agent at all.
    #[tokio::test]
    async fn a_request_without_a_token_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tokens = Tokens::at(&dir.path().join("http_tokens.json"));
        let addr = start(tokens, "no-agent".into()).await;
        let response = client()
            .post(format!("http://{addr}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    }
}
