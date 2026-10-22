//! Fetches the history archive a phone uploads for a newly linked device.
//! Neither presage nor libsignal-service speaks these two calls yet
//! (docs/link-and-sync.md), so this does, over its own HTTP client.

use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::StatusCode;
use serde::Deserialize;
use url::Url;

/// Where the phone said its archive went, or why it sent none.
#[derive(Debug, PartialEq, Eq)]
enum Waited {
    Ready { cdn: u32, key: String },
    Relink,
    NoUpload,
}

/// The new device's view of Signal's servers, enough to fetch one archive.
struct ArchiveSource {
    http: reqwest::Client,
    login: (String, String),
    hosts: Hosts,
    patience: Patience,
}

enum Hosts {
    #[cfg(test)]
    Local(Url),
}

/// How long to wait on the phone.
struct Patience {
    per_poll: Duration,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ArchiveResponse {
    Ready { cdn: u32, key: String },
    Failed { error: String },
}

/// A client for Signal's hosts, configured by `builder`.
fn http_client(builder: reqwest::ClientBuilder) -> anyhow::Result<reqwest::Client> {
    // libsignal-service installs the same provider when presage first connects.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    builder
        .connect_timeout(Duration::from_secs(10))
        .http1_only()
        .build()
        .context("unable to set up an HTTP client for Signal")
}

impl ArchiveSource {
    fn chat_url(&self, path: &str) -> anyhow::Result<Url> {
        match &self.hosts {
            #[cfg(test)]
            Hosts::Local(base) => Ok(base.join(path)?),
        }
    }

    /// Long-polls until the phone says where the archive is.
    async fn wait_for_archive(&self) -> anyhow::Result<Waited> {
        let mut url = self.chat_url("/v1/devices/transfer_archive")?;
        let secs = self.patience.per_poll.as_secs().max(1);
        url.query_pairs_mut()
            .append_pair("timeout", &secs.to_string());
        let (user, password) = &self.login;
        loop {
            let response = self
                .http
                .get(url.clone())
                .basic_auth(user, Some(password))
                .send()
                .await
                .context("unable to ask Signal for the history archive")?;
            if response.status() == StatusCode::NO_CONTENT {
                continue;
            }
            if !response.status().is_success() {
                bail!(
                    "Signal refused the history archive request: {}",
                    response.status()
                );
            }
            let answer = response
                .json()
                .await
                .context("Signal's answer about the history archive is unreadable")?;
            return match answer {
                ArchiveResponse::Ready { cdn, key } => Ok(Waited::Ready { cdn, key }),
                ArchiveResponse::Failed { error } => match error.as_str() {
                    "RELINK_REQUESTED" => Ok(Waited::Relink),
                    "CONTINUE_WITHOUT_UPLOAD" => Ok(Waited::NoUpload),
                    _ => bail!("the phone failed to send its history: {error}"),
                },
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    /// A stand-in for Signal's chat service and CDN on a loopback port: it
    /// records each request's head and answers with the next canned response,
    /// repeating the last one once the list runs out.
    struct FakeServer {
        url: Url,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl FakeServer {
        async fn start(responses: Vec<Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let url = Url::parse(&format!("http://{}/", listener.local_addr().expect("addr")))
                .expect("url");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            tokio::spawn(async move {
                let mut next = 0;
                while let Ok((mut socket, _)) = listener.accept().await {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match socket.read(&mut byte).await {
                            Ok(1) => head.push(byte[0]),
                            _ => break,
                        }
                    }
                    seen.lock()
                        .expect("requests")
                        .push(String::from_utf8_lossy(&head).into_owned());
                    let response = &responses[next.min(responses.len() - 1)];
                    next += 1;
                    let _ = socket.write_all(response).await;
                    let _ = socket.shutdown().await;
                }
            });
            Self { url, requests }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("requests").clone()
        }
    }

    fn json(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn source(server: &FakeServer) -> ArchiveSource {
        ArchiveSource {
            http: http_client(reqwest::Client::builder().no_proxy()).expect("client"),
            login: ("me.2".into(), "secret".into()),
            hosts: Hosts::Local(server.url.clone()),
            patience: Patience {
                per_poll: Duration::from_secs(1),
            },
        }
    }

    /// The phone's answer names the CDN and key the archive went to; the
    /// request must be the long-poll, authenticated as the new device.
    #[tokio::test]
    async fn a_ready_archive_is_located_by_an_authenticated_long_poll() {
        let server = FakeServer::start(vec![json("200 OK", r#"{"cdn":3,"key":"a/b+c"}"#)]).await;
        let waited = source(&server).wait_for_archive().await.expect("waited");
        assert_eq!(
            waited,
            Waited::Ready {
                cdn: 3,
                key: "a/b+c".into()
            }
        );
        let requests = server.requests();
        let [request] = requests.as_slice() else {
            panic!("expected one request: {requests:?}");
        };
        assert!(
            request.starts_with("GET /v1/devices/transfer_archive?timeout=1 HTTP/1.1"),
            "{request}"
        );
        // base64("me.2:secret")
        assert!(
            request
                .to_lowercase()
                .contains("authorization: basic bwuumjpzzwnyzxq="),
            "{request}"
        );
    }

    fn empty(status: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").into_bytes()
    }

    /// 204 is the server saying the phone hasn't answered yet, so the
    /// device must ask again rather than give up.
    #[tokio::test]
    async fn no_content_means_ask_again() {
        let server = FakeServer::start(vec![
            empty("204 No Content"),
            empty("204 No Content"),
            json("200 OK", r#"{"cdn":2,"key":"k"}"#),
        ])
        .await;
        let waited = source(&server).wait_for_archive().await.expect("waited");
        assert_eq!(
            waited,
            Waited::Ready {
                cdn: 2,
                key: "k".into()
            }
        );
        assert_eq!(server.requests().len(), 3, "{:?}", server.requests());
    }

    /// The phone's two failure answers mean different things to the user:
    /// link again, or carry on without history. Neither is an error.
    #[tokio::test]
    async fn the_phones_error_answers_say_relink_or_carry_on() {
        for (body, expected) in [
            (r#"{"error":"RELINK_REQUESTED"}"#, Waited::Relink),
            (r#"{"error":"CONTINUE_WITHOUT_UPLOAD"}"#, Waited::NoUpload),
        ] {
            let server = FakeServer::start(vec![json("200 OK", body)]).await;
            let waited = source(&server).wait_for_archive().await.expect(body);
            assert_eq!(waited, expected, "{body}");
        }
    }
}
