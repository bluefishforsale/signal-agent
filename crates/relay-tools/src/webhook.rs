//! The webhook address the agent posts incoming messages to. Every message's
//! contents leave the machine through it, so it's validated once, here, and
//! only the user sets it, in Settings.

use std::{fmt, net::IpAddr};

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::{Host, Url};

use crate::{Tool, ToolContext, ToolError, input::parse};

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

// NOTE: a read only. There is deliberately no set_webhook tool (PRD anti-goal
// 13): a client that could point the webhook somewhere would be one prompt
// injection away from forwarding every incoming message off the machine.
pub struct GetWebhook;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetWebhookInput {}

#[async_trait]
impl Tool for GetWebhook {
    fn name(&self) -> &'static str {
        "get_webhook"
    }
    fn description(&self) -> &'static str {
        "Show the webhook URL incoming messages are forwarded to, or null when none is set. \
         Only the user sets it, in the app's Advanced settings; no tool can change it."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let GetWebhookInput {} = parse(input)?;
        Ok(json!({ "webhook": ctx.webhook }))
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

    /// get_webhook is how a client learns where messages are forwarded, so it
    /// must report the setting even on a read-only profile, and null when unset.
    #[tokio::test]
    async fn get_webhook_reports_the_setting_on_a_read_only_profile() {
        use relay_core::prelude::*;
        use serde_json::json;

        use crate::{Registry, testing::ctx};

        let (unset, _) = ctx(AccessMode::ReadOnly, ProfileKind::Primary);
        let set = ToolContext {
            webhook: Some(WebhookUrl::parse("https://example.com/hook").unwrap()),
            ..unset.clone()
        };
        let registry = Registry::standard();
        let out = registry.call(&set, "get_webhook", json!({})).await.unwrap();
        assert_eq!(out, json!({"webhook": "https://example.com/hook"}));
        let out = registry
            .call(&unset, "get_webhook", json!({}))
            .await
            .unwrap();
        assert_eq!(out, json!({"webhook": null}));
    }
}
