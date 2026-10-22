//! The contract the presage implementation fulfills. Kept to what the agent
//! needs; protocol detail stays below this line.

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use relay_core::prelude::*;
use serde::{Deserialize, Serialize};

/// The account a registered backend operates as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub service_id: ServiceId,
    pub phone_number: Option<String>,
    pub kind: ProfileKind,
    pub device_name: String,
}

/// A contact as known to the account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    pub service_id: ServiceId,
    pub name: Option<String>,
    pub phone_number: Option<String>,
}

/// A group the account is a member of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub id: GroupId,
    pub title: String,
    pub members: Vec<ServiceId>,
}

/// Something to send. Reactions, edits and deletes reference the target by its
/// `MessageId`, as Signal does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    Text {
        body: String,
        quote: Option<MessageId>,
    },
    Reaction {
        target: MessageId,
        emoji: String,
        remove: bool,
    },
    Edit {
        target: MessageId,
        body: String,
    },
    Delete {
        target: MessageId,
    },
    Typing {
        started: bool,
    },
    ReadReceipt {
        targets: Vec<MessageId>,
    },
}

/// Something that arrived, from another person or synced from the account's
/// own other devices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    Message(Message),
    Reaction {
        conversation: Recipient,
        target: MessageId,
        from: ServiceId,
        emoji: String,
        remove: bool,
    },
    // NOTE: `from` is who sent the edit or delete. The store must refuse it
    // unless `from` is the target's author, or anyone in a chat could rewrite or
    // erase someone else's messages in the local history.
    Edit {
        conversation: Recipient,
        target: MessageId,
        from: ServiceId,
        body: String,
    },
    Delete {
        conversation: Recipient,
        target: MessageId,
        from: ServiceId,
    },
    Read {
        targets: Vec<MessageId>,
    },
    /// A contact's identity key (safety number) changed, usually a reinstall or
    /// new phone. The backend trusts the new key as Signal's own apps do, so
    /// messaging keeps working; this event is how the change stays visible.
    IdentityChanged {
        contact: ServiceId,
    },
}

/// Signal refused a request as rate-limited, or wants proof that a person is
/// behind the account before it accepts more. Nothing retries the request.
// NOTE: this is the one typed error at the Signal seam, because tools and setup
// must tell it apart: retrying a request Signal rate-limited is how accounts
// get flagged. Everything else stays anyhow.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "Signal refused this as rate-limited, and it was not retried ({message}). {}",
    wait_hint(*retry_after)
)]
pub struct RateLimited {
    /// What Signal said.
    pub message: String,
    /// How long Signal asked to wait, when it said.
    pub retry_after: Option<Duration>,
}

fn wait_hint(retry_after: Option<Duration>) -> String {
    match retry_after {
        Some(wait) => format!("Try again in {}s", wait.as_secs()),
        None => "Wait a while before trying again".into(),
    }
}

/// A registered account's live connection to Signal.
#[async_trait]
pub trait SignalBackend: Send + Sync {
    async fn account(&self) -> anyhow::Result<Account>;
    async fn send(&self, to: Recipient, message: Outgoing) -> anyhow::Result<MessageId>;
    async fn contacts(&self) -> anyhow::Result<Vec<Contact>>;
    async fn groups(&self) -> anyhow::Result<Vec<Group>>;
    async fn attachment(&self, attachment: &AttachmentRef) -> anyhow::Result<Vec<u8>>;
    /// Everything that arrives from now on. Called once by the agent, which is
    /// the only consumer.
    fn incoming(&self) -> BoxStream<'static, anyhow::Result<Incoming>>;
}

/// Progress while linking as a secondary device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LinkEvent {
    /// Show this `sgnl://linkdevice` URI as a QR code. Replaces any earlier one.
    Uri { uri: String },
    /// The phone accepted; the account is ready.
    Linked { account: Account },
}

/// Progress while registering a number as a primary device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RegistrationStep {
    /// Signal wants a captcha token before it will send a code.
    CaptchaRequired,
    /// A code is on its way by SMS or voice call.
    CodeSent,
    Registered {
        account: Account,
    },
}

/// Creates an account in a profile's empty store: by linking to an existing
/// account, or by registering a number.
#[async_trait]
pub trait Provisioner: Send + Sync {
    fn link(&self, device_name: String) -> BoxStream<'static, anyhow::Result<LinkEvent>>;
    async fn request_code(
        &self,
        phone_number: &str,
        captcha: Option<&str>,
        voice: bool,
    ) -> anyhow::Result<RegistrationStep>;
    async fn verify_code(&self, code: &str) -> anyhow::Result<RegistrationStep>;
}
