//! The webhook address the agent posts incoming messages to. Every message's
//! contents leave the machine through it, so it's validated once, here, and
//! only the user sets it, in Settings.

use std::{fmt, net::IpAddr};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use url::{Host, Url};

/// An `https` URL, or an `http` one on this machine's loopback.
// NOTE: plain http off loopback would send message contents, and often a token
// in the URL, across the network in the clear. Same rule as the HTTP endpoint:
// no TLS, no leaving the machine.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WebhookUrl(Url);

impl WebhookUrl {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let url = Url::parse(s.trim()).with_context(|| format!("\"{s}\" is not a URL"))?;
        match url.scheme() {
            "https" => {}
            "http" if is_loopback(&url) => {}
            "http" => bail!("webhook \"{s}\" must use https unless it's on this machine"),
            other => bail!("webhook \"{s}\" must be http or https, not {other}"),
        }
        Ok(Self(url))
    }

    pub fn as_url(&self) -> &Url {
        &self.0
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        None => false,
    }
}

impl fmt::Display for WebhookUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for WebhookUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WebhookUrl({})", self.0)
    }
}

impl TryFrom<String> for WebhookUrl {
    type Error = anyhow::Error;
    fn try_from(s: String) -> anyhow::Result<Self> {
        Self::parse(&s)
    }
}

impl From<WebhookUrl> for String {
    fn from(w: WebhookUrl) -> String {
        w.0.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Message contents may only leave the machine encrypted: https anywhere,
    /// plain http only to loopback, nothing else at all.
    #[test]
    fn a_webhook_leaves_the_machine_only_over_https() {
        for ok in [
            "https://example.com/hook?token=abc",
            "http://localhost:8080/hook",
            "http://127.0.0.1/hook",
            "http://[::1]:9000/",
        ] {
            assert!(WebhookUrl::parse(ok).is_ok(), "refused {ok}");
        }
        for bad in [
            "http://example.com/hook",
            "http://192.168.1.10/hook",
            "http://localhost.example.com/",
            "ftp://localhost/",
            "file:///etc/passwd",
            "not a url",
            "",
        ] {
            assert!(WebhookUrl::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    /// A stored value must be revalidated on the way back in, so a hand-edited
    /// row can't smuggle in a plain-http remote address.
    #[test]
    fn deserializing_a_webhook_validates_it() {
        let bad: Result<WebhookUrl, _> = serde_json::from_str("\"http://example.com/\"");
        assert!(bad.is_err(), "deserialized an insecure webhook: {bad:?}");
    }
}
