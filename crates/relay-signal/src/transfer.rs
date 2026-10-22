//! Fetches the history archive a phone uploads for a newly linked device.
//! Neither presage nor libsignal-service speaks these two calls yet
//! (docs/link-and-sync.md), so this does, over its own HTTP client.

use std::{
    io::SeekFrom,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use reqwest::{
    StatusCode,
    header::{RANGE, RETRY_AFTER},
};
use serde::Deserialize;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use url::Url;

use crate::RateLimited;

/// Where the phone said its archive went, or why it sent none.
#[derive(Debug, PartialEq, Eq)]
enum Waited {
    Ready { cdn: u32, key: String },
    Relink,
    NoUpload,
    TimedOut,
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
    /// The longest Signal is asked to hold one poll open.
    per_poll: Duration,
    /// When to stop asking.
    total: Duration,
    /// The least time between the starts of two polls, whatever the server
    /// does with `per_poll`.
    min_gap: Duration,
    /// How many times to try the download when the connection breaks.
    download_attempts: u32,
    /// How long to wait before resuming a broken download.
    retry_delay: Duration,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ArchiveResponse {
    Ready { cdn: u32, key: String },
    Failed { error: String },
}

/// How much longer than the poll's own timeout to wait for Signal's answer.
const POLL_SLACK: Duration = Duration::from_secs(15);

/// Why Signal refused `response`, as the typed `RateLimited` for a 429.
fn refused(response: &reqwest::Response, what: &str) -> anyhow::Error {
    let status = response.status();
    if status != StatusCode::TOO_MANY_REQUESTS {
        return anyhow!("Signal refused {what}: {status}");
    }
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok()?.trim().parse().ok())
        .map(Duration::from_secs);
    RateLimited {
        message: format!("{what}: {status}"),
        retry_after,
    }
    .into()
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

    fn cdn_url(&self, cdn: u32, key: &str) -> anyhow::Result<Url> {
        let mut url = match &self.hosts {
            #[cfg(test)]
            Hosts::Local(base) => base.join(&format!("cdn{cdn}/attachments"))?,
        };
        url.path_segments_mut()
            .map_err(|()| anyhow!("CDN {cdn} has no path for attachments"))?
            .push(key);
        Ok(url)
    }

    /// Downloads the archive the phone put on `cdn` under `key` to `to`,
    /// resuming where it stopped when the connection breaks.
    async fn download(&self, cdn: u32, key: &str, to: &Path) -> anyhow::Result<()> {
        let url = self.cdn_url(cdn, key)?;
        let mut file = tokio::fs::File::create(to)
            .await
            .with_context(|| format!("unable to create \"{}\"", to.display()))?;
        let mut written = 0u64;
        let mut attempt = 1;
        loop {
            let broke = match self.fetch_from(&url, &mut file, &mut written).await? {
                None => return Ok(()),
                Some(broke) => broke,
            };
            if attempt >= self.patience.download_attempts {
                return Err(broke.context("the history archive download kept breaking off"));
            }
            attempt += 1;
            tokio::time::sleep(self.patience.retry_delay).await;
        }
    }

    /// Fetches `url` from byte `written` on into `file`. The outer error is
    /// Signal's refusal; the inner one a connection that broke, worth resuming.
    async fn fetch_from(
        &self,
        url: &Url,
        file: &mut tokio::fs::File,
        written: &mut u64,
    ) -> anyhow::Result<Option<anyhow::Error>> {
        let mut request = self.http.get(url.clone());
        if *written > 0 {
            request = request.header(RANGE, format!("bytes={written}-"));
        }
        let mut response = match request.send().await {
            Ok(response) => response,
            Err(e) => return Ok(Some(anyhow!(e))),
        };
        match response.status() {
            StatusCode::PARTIAL_CONTENT => {}
            StatusCode::OK => {
                // The CDN ignored the range and sent everything again.
                file.set_len(0).await?;
                file.seek(SeekFrom::Start(0)).await?;
                *written = 0;
            }
            _ => return Err(refused(&response, "the history archive download")),
        }
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => return Ok(Some(anyhow!(e))),
            };
            file.write_all(&chunk)
                .await
                .context("unable to write the history archive")?;
            *written += chunk.len() as u64;
        }
        file.flush()
            .await
            .context("unable to write the history archive")?;
        Ok(None)
    }

    /// Long-polls until the phone says where the archive is.
    async fn wait_for_archive(&self) -> anyhow::Result<Waited> {
        let url = self.chat_url("/v1/devices/transfer_archive")?;
        let (user, password) = &self.login;
        let deadline = Instant::now() + self.patience.total;
        loop {
            let started = Instant::now();
            let remaining = deadline.saturating_duration_since(started);
            if remaining.is_zero() {
                return Ok(Waited::TimedOut);
            }
            let secs = self.patience.per_poll.min(remaining).as_secs().max(1);
            let mut url = url.clone();
            url.query_pairs_mut()
                .append_pair("timeout", &secs.to_string());
            let response = self
                .http
                .get(url)
                .basic_auth(user, Some(password))
                .timeout(Duration::from_secs(secs) + POLL_SLACK)
                .send()
                .await
                .context("unable to ask Signal for the history archive")?;
            if response.status() == StatusCode::NO_CONTENT {
                tokio::time::sleep_until((started + self.patience.min_gap).into()).await;
                continue;
            }
            if !response.status().is_success() {
                return Err(refused(&response, "the history archive request"));
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
                total: Duration::from_secs(10),
                min_gap: Duration::ZERO,
                download_attempts: 3,
                retry_delay: Duration::ZERO,
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

    /// A 429 on the long-poll must come back as the typed RateLimited with
    /// Signal's wait, and must not be asked again: retrying a rate-limited
    /// request is how accounts get flagged.
    #[tokio::test]
    async fn a_rate_limited_poll_is_not_retried_and_carries_signals_wait() {
        let server = FakeServer::start(vec![
            b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 90\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
            json("200 OK", r#"{"cdn":2,"key":"k"}"#),
        ])
        .await;
        let error = source(&server)
            .wait_for_archive()
            .await
            .expect_err("a 429 was not an error");
        let limited = error
            .downcast_ref::<RateLimited>()
            .unwrap_or_else(|| panic!("not RateLimited: {error:#}"));
        assert_eq!(
            limited.retry_after,
            Some(Duration::from_secs(90)),
            "{limited:?}"
        );
        assert_eq!(server.requests().len(), 1, "{:?}", server.requests());
    }

    /// The device gives up after the total wait, as Signal's apps do after an
    /// hour, and a server that answers 204 at once must not be hammered.
    #[tokio::test]
    async fn waiting_gives_up_after_the_total_wait_without_hammering() {
        let server = FakeServer::start(vec![empty("204 No Content")]).await;
        let mut source = source(&server);
        source.patience.total = Duration::from_millis(500);
        source.patience.min_gap = Duration::from_millis(200);
        let waited = source.wait_for_archive().await.expect("waited");
        assert_eq!(waited, Waited::TimedOut);
        let polls = server.requests().len();
        assert!((2..=3).contains(&polls), "polled {polls} times in 500ms");
    }

    fn bytes(status: &str, extra_headers: &str, body: &[u8], content_length: usize) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\n{extra_headers}content-length: {content_length}\r\nconnection: close\r\n\r\n"
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// The archive is an ordinary attachment on the CDN the phone named,
    /// fetched under its URL-encoded key without the device's credentials.
    #[tokio::test]
    async fn the_archive_downloads_from_the_named_cdn_without_credentials() {
        let archive = b"encrypted archive bytes".to_vec();
        let server = FakeServer::start(vec![bytes("200 OK", "", &archive, archive.len())]).await;
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("archive");
        source(&server)
            .download(3, "a/b c", &path)
            .await
            .expect("downloaded");
        assert_eq!(std::fs::read(&path).expect("file"), archive);
        let requests = server.requests();
        let [request] = requests.as_slice() else {
            panic!("expected one request: {requests:?}");
        };
        assert!(
            request.starts_with("GET /cdn3/attachments/a%2Fb%20c HTTP/1.1"),
            "{request}"
        );
        assert!(
            !request.to_lowercase().contains("authorization"),
            "{request}"
        );
    }

    /// A download cut off partway must pick up where it stopped with a Range
    /// request, as Signal Desktop does, rather than refetch or keep a
    /// truncated file.
    #[tokio::test]
    async fn a_broken_download_resumes_from_where_it_stopped() {
        let archive: Vec<u8> = (0..=255).collect();
        let (head, tail) = archive.split_at(100);
        let server = FakeServer::start(vec![
            bytes("200 OK", "", head, archive.len()),
            bytes(
                "206 Partial Content",
                "content-range: bytes 100-255/256\r\n",
                tail,
                tail.len(),
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("archive");
        source(&server)
            .download(0, "k", &path)
            .await
            .expect("downloaded");
        assert_eq!(std::fs::read(&path).expect("file"), archive);
        let requests = server.requests();
        let [_, resumed] = requests.as_slice() else {
            panic!("expected two requests: {requests:?}");
        };
        assert!(
            resumed.to_lowercase().contains("range: bytes=100-\r\n"),
            "{resumed}"
        );
    }
}
