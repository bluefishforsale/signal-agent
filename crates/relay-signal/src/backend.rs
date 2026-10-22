//! The contract the presage implementation fulfills. Kept to what the agent
//! needs; protocol detail stays below this line.

use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use futures::stream::BoxStream;
use relay_core::{LinkAndSyncKey, prelude::*};
use serde::{Deserialize, Serialize};

use crate::SendableFile;

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
    /// Members allowed to administer the group, including deleting other
    /// members' messages for everyone.
    pub admins: Vec<ServiceId>,
}

/// A stored identity key for one of the account's contacts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub service_id: ServiceId,
    /// The safety number shown in Signal's apps, as digits.
    pub safety_number: String,
    pub trust: Trust,
    /// True when this key replaced an earlier one and hasn't been verified
    /// since: the contact's safety number changed.
    pub changed: bool,
}

/// How far an identity key is trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Verified,
    Unverified,
    Untrusted,
}

/// A device linked to the account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// 1 is the primary.
    pub id: u32,
    pub name: Option<String>,
    pub created_at_ms: u64,
    pub last_seen_at_ms: u64,
}

/// Something to send. Reactions, edits and deletes reference the target by its
/// `MessageId`, as Signal does.
// NOTE: every variant here must be classified in relay-tools' limits.rs
// `counts()`, whose match is exhaustive on purpose: a new variant that reaches
// a person but slips past the send limiter is how an account gets flagged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    /// `attachments` are refs `SignalBackend::upload` returned; an empty list
    /// is a plain text message.
    Text {
        body: String,
        quote: Option<MessageId>,
        #[serde(default)]
        attachments: Vec<AttachmentRef>,
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
    /// A sticker from a pack installed on the account.
    Sticker {
        pack: StickerPackId,
        sticker_id: u32,
    },
    /// Pins `target` for everyone in the conversation, for `duration_secs` or
    /// until unpinned when `None`.
    Pin {
        target: MessageId,
        duration_secs: Option<NonZeroU32>,
    },
    Unpin {
        target: MessageId,
    },
    PollCreate {
        question: String,
        options: Vec<String>,
        multi_select: bool,
    },
    /// `vote_count` must exceed every earlier vote's on the same poll; Signal's
    /// apps keep only the highest.
    PollVote {
        poll: MessageId,
        option_indexes: Vec<u32>,
        vote_count: u32,
    },
    /// Closes a poll this account created.
    PollTerminate {
        poll: MessageId,
    },
    /// Deletes another member's group message for everyone, on this account's
    /// authority as a group admin. Only sendable to a group.
    AdminDelete {
        target: MessageId,
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
    // NOTE: `from` claims to be a group admin. Whoever applies this must check
    // `from` against the group's admins from `SignalBackend::groups` before
    // calling `MessageStore::mark_admin_deleted`, or any member could erase
    // anyone's messages from the local history.
    AdminDelete {
        group: GroupId,
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

/// This version can't look phone numbers up on Signal.
// NOTE: typed so a tool can say so plainly instead of reporting every number
// as unregistered. Discovery needs presage's cdsi feature, held back for now
// (BoringSSL on every target, and bulk lookups look like contact scraping).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("looking up phone numbers on Signal isn't supported in this version")]
pub struct DiscoveryUnsupported;

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

    /// Encrypts and uploads `file` so an `Outgoing::Text` can carry it. Nobody
    /// receives anything until that message is sent.
    async fn upload(&self, file: &SendableFile) -> anyhow::Result<AttachmentRef>;
    /// Which of `numbers` (E.164) are registered on Signal, in the order
    /// given; `None` when a number isn't, or hides itself from discovery.
    async fn discover(
        &self,
        numbers: &[String],
    ) -> anyhow::Result<Vec<(String, Option<ServiceId>)>>;
    /// The account a Signal username belongs to, if any.
    async fn lookup_username(&self, username: &str) -> anyhow::Result<Option<ServiceId>>;
    /// A contact's profile picture or a group's avatar as image bytes; `None`
    /// when they have none.
    async fn avatar(&self, of: Recipient) -> anyhow::Result<Option<Vec<u8>>>;
    /// Every stored identity key for the account's contacts.
    async fn identities(&self) -> anyhow::Result<Vec<Identity>>;
    /// Every device linked to the account, as Signal's servers report them.
    async fn devices(&self) -> anyhow::Result<Vec<Device>>;
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

/// How the history transfer the phone may offer at link time ended.
#[derive(Debug)]
pub enum HistoryTransfer {
    /// The archive is downloaded to `path`, encrypted under `key`. The caller
    /// imports it, then deletes the file.
    Archive { path: PathBuf, key: LinkAndSyncKey },
    /// The phone linked without offering its history.
    NotOffered,
    /// The phone failed to send its history and asks the user to link again.
    Relink,
    /// The phone gave up on sending its history; carry on without it.
    NoUpload,
    /// The phone said nothing within the hour Signal's own apps wait.
    TimedOut,
}

/// Creates an account in a profile's empty store: by linking to an existing
/// account, or by registering a number.
#[async_trait]
pub trait Provisioner: Send + Sync {
    fn link(&self, device_name: String) -> BoxStream<'static, anyhow::Result<LinkEvent>>;
    /// After `link` reports `Linked`, waits for the history the phone offered
    /// to send and downloads it into `dir`. Fails with `RateLimited` when
    /// Signal refuses as rate-limited, and never retries that itself. After a
    /// failure the caller may call again; inside Signal's wait that fails
    /// with `RateLimited` without asking Signal.
    async fn transfer_history(&self, _dir: &Path) -> anyhow::Result<HistoryTransfer> {
        Ok(HistoryTransfer::NotOffered)
    }
    async fn request_code(
        &self,
        phone_number: &str,
        captcha: Option<&str>,
        voice: bool,
    ) -> anyhow::Result<RegistrationStep>;
    async fn verify_code(&self, code: &str) -> anyhow::Result<RegistrationStep>;
}
