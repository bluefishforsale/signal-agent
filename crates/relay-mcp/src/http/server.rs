//! The HTTP listener and the gate in front of it. A request gets nowhere
//! without a live token, and the token's scope picks which agent connection
//! serves it, so a read-only token meets the registry's read-only gate.

use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use anyhow::{Context, bail};
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
use rustls::ServerConfig;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
};
use tokio_rustls::TlsAcceptor;

use super::{Tokens, proxy::Proxy};

/// A bound HTTP endpoint, not yet serving.
pub struct Listener {
    tcp: TcpListener,
    tls: Option<TlsAcceptor>,
}

impl Listener {
    /// Listens on `addr`, speaking TLS with `tls` when given. Plain HTTP is
    /// only allowed on loopback, since every request carries a token.
    pub async fn bind(addr: SocketAddr, tls: Option<Arc<ServerConfig>>) -> anyhow::Result<Self> {
        if tls.is_none() && !addr.ip().is_loopback() {
            bail!("refusing to listen on {addr} without TLS; only loopback may use plain HTTP");
        }
        let tcp = TcpListener::bind(addr)
            .await
            .with_context(|| format!("unable to listen on {addr}"))?;
        Ok(Self {
            tcp,
            tls: tls.map(TlsAcceptor::from),
        })
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
        let (tcp, peer) = listener.tcp.accept().await?;
        let gate = gate.clone();
        let tls = listener.tls.clone();
        tokio::spawn(async move {
            let served = match tls {
                None => connection(tcp, peer, gate).await,
                Some(tls) => match tls.accept(tcp).await {
                    Ok(stream) => connection(stream, peer, gate).await,
                    Err(e) => return tracing::debug!(%peer, "TLS handshake failed: {e}"),
                },
            };
            if let Err(e) = served {
                tracing::debug!(%peer, "HTTP connection ended: {e}");
            }
        });
    }
}

async fn connection<S>(stream: S, peer: SocketAddr, gate: Arc<Gate>) -> hyper::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |request| {
        let gate = gate.clone();
        async move { Ok::<_, Infallible>(gate.handle(peer, request).await) }
    });
    hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
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
    async fn handle(&self, peer: SocketAddr, request: Request<Incoming>) -> Response<Body> {
        let (method, path) = (request.method().clone(), request.uri().path().to_owned());
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
            Some(Ok(None)) | None => {
                tracing::warn!(%peer, %method, path, "refused an HTTP request without a live token");
                return unauthorized();
            }
        };
        let service = match grant.scope {
            AccessMode::Full => &self.full,
            AccessMode::ReadOnly => &self.read_only,
        };
        let response = service.handle(request).await;
        let status = response.status().as_u16();
        tracing::info!(token = grant.name, %peer, %method, path, status, "HTTP request");
        response
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
        let listener = Listener::bind(([127, 0, 0, 1], 0).into(), None)
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

    async fn post(addr: SocketAddr, token: Option<&str>) -> reqwest::StatusCode {
        let mut request = client()
            .post(format!("http://{addr}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.expect("request").status()
    }

    /// PRD acceptance 10: a request with no token must be refused before it
    /// reaches the agent at all.
    #[tokio::test]
    async fn a_request_without_a_token_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tokens = Tokens::at(&dir.path().join("http_tokens.json"));
        let addr = start(tokens, "no-agent".into()).await;
        assert_eq!(post(addr, None).await, reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            post(addr, Some("guess")).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
    }

    /// PRD acceptance 10: revoking a token must lock its client out on the
    /// very next request, without restarting the endpoint.
    #[tokio::test]
    async fn a_token_revoked_while_serving_is_refused_on_its_next_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (agent, _) = agent(dir.path(), Registry::default().with(ProbeSend)).await;
        let path = dir.path().join("http_tokens.json");
        let token = Tokens::at(&path)
            .create("laptop", AccessMode::Full)
            .expect("create");
        let addr = start(Tokens::at(&path), agent).await;
        let client = remote(addr, &token).await;
        client
            .peer()
            .list_tools(None)
            .await
            .expect("before revoking");
        Tokens::at(&path).revoke("laptop").expect("revoke");
        assert_eq!(
            post(addr, Some(&token)).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
    }

    /// PRD acceptance 10, swept over every shipped tool: a read-only token
    /// must not see or run any write tool, even though the agent behind it
    /// has full access. New tools are covered by being registered.
    #[tokio::test]
    async fn a_read_only_token_can_neither_see_nor_call_any_write_tool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (agent, signal) = agent(dir.path(), Registry::standard()).await;
        let tokens = Tokens::at(&dir.path().join("http_tokens.json"));
        let token = tokens
            .create("phone", AccessMode::ReadOnly)
            .expect("create");
        let addr = start(tokens, agent).await;
        let client = remote(addr, &token).await;
        let registry = Registry::standard();
        let writes: Vec<_> = registry
            .all()
            .filter(|t| t.writes())
            .map(|t| t.name())
            .collect();
        assert!(!writes.is_empty(), "no write tools to sweep");
        let listed = client.peer().list_tools(None).await.expect("tools/list");
        let leaked: Vec<_> = listed
            .tools
            .iter()
            .filter(|t| writes.contains(&t.name.as_ref()))
            .map(|t| t.name.to_string())
            .collect();
        assert!(
            leaked.is_empty(),
            "write tools listed read-only: {leaked:?}"
        );
        for name in writes {
            let result = client
                .peer()
                .call_tool(CallToolRequestParams::new(name))
                .await;
            let refused = result
                .as_ref()
                .is_ok_and(|r| r.is_error == Some(true) && format!("{r:?}").contains("read-only"));
            assert!(refused, "{name} over a read-only token: {result:?}");
        }
        assert!(signal.sent().is_empty(), "a read-only token reached Signal");
    }

    /// PRD acceptance 10: tokens cross the network in every request, so the
    /// endpoint must refuse to listen anywhere but loopback in plain HTTP.
    // NOTE: 192.0.2.1 is TEST-NET-1, so a regression can't actually bind
    // off loopback; the assertion is on the reason, not just the failure.
    #[tokio::test]
    async fn an_off_loopback_address_without_tls_is_refused() {
        let refused = Listener::bind(([192, 0, 2, 1], 0).into(), None).await;
        let Err(e) = refused else {
            panic!("listened off loopback without TLS")
        };
        assert!(format!("{e:#}").contains("TLS"), "unclear refusal: {e:#}");
    }
}
