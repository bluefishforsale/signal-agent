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
    use super::*;

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
