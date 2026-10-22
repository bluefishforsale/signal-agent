//! Tools that act on a message already in a conversation: reactions, edits and
//! deletes. Signal names the target by author and send timestamp, and the local
//! store follows only once Signal has accepted the change.

use anyhow::Context;
use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::Outgoing;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{contact, contact_schema, non_blank, parse, recipient, recipient_schema},
};

/// A message's send timestamp in epoch ms, as Signal uses it to name the message.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "u64")]
struct Timestamp(u64);

impl Timestamp {
    // JavaScript's largest safe integer, so every MCP client can send it exactly.
    const MAX: u64 = (1 << 53) - 1;

    fn schema(description: &str) -> Value {
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": Self::MAX,
            "description": format!("{description}: its send timestamp in epoch milliseconds, \
                                    as get_conversation returns it"),
        })
    }
}

impl TryFrom<u64> for Timestamp {
    type Error = String;
    fn try_from(n: u64) -> Result<Self, String> {
        match n {
            1..=Self::MAX => Ok(Self(n)),
            _ => Err(format!(
                "target_timestamp must be 1 to {}, got {n}",
                Self::MAX
            )),
        }
    }
}

pub struct ReactToMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactInput {
    recipient: String,
    target_author: String,
    target_timestamp: Timestamp,
    emoji: String,
    #[serde(default)]
    remove: bool,
}

#[async_trait]
impl Tool for ReactToMessage {
    fn name(&self) -> &'static str {
        "react_to_message"
    }
    fn description(&self) -> &'static str {
        "Add or remove your emoji reaction on a message in a DM or group, to acknowledge \
         without replying. recipient is the conversation (as list_conversations names it, \
         group ids included); target_author and target_timestamp name the message, as \
         get_conversation returns it. You have one reaction per message: a new emoji replaces \
         the old one. remove=true retracts that emoji. Sends to Signal; nothing is stored \
         locally."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "target_author": contact_schema("Who wrote the message"),
                "target_timestamp": Timestamp::schema("The message to react to"),
                "emoji": {"type": "string", "description": "The emoji, e.g. 👍"},
                "remove": {
                    "type": "boolean",
                    "default": false,
                    "description": "Retract this emoji instead of adding it (default false)",
                },
            },
            "required": ["recipient", "target_author", "target_timestamp", "emoji"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: ReactInput = parse(input)?;
        let emoji = non_blank("emoji", input.emoji)?;
        let to = recipient(ctx, &input.recipient).await?;
        let target = MessageId {
            author: contact(ctx, &input.target_author).await?,
            sent_at_ms: input.target_timestamp.0,
        };
        let remove = input.remove;
        ctx.signal
            .send(
                to,
                Outgoing::Reaction {
                    target,
                    emoji,
                    remove,
                },
            )
            .await?;
        let status = if remove {
            "reaction removed"
        } else {
            "reaction sent"
        };
        Ok(json!({ "status": status }))
    }
}

pub struct EditMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    recipient: String,
    target_timestamp: Timestamp,
    message: String,
}

#[async_trait]
impl Tool for EditMessage {
    fn name(&self) -> &'static str {
        "edit_message"
    }
    fn description(&self) -> &'static str {
        "Replace the text of a message you sent, in a DM or group; recipients see it marked \
         edited. recipient is the conversation (group ids included); target_timestamp names \
         your message, as get_conversation or a send result returns it; message is the new \
         full text. Only your own messages can be edited. Sends to Signal, then overwrites the \
         stored body (the old text isn't kept)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "target_timestamp": Timestamp::schema("Your message to edit"),
                "message": {"type": "string", "description": "The new full text"},
            },
            "required": ["recipient", "target_timestamp", "message"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: EditInput = parse(input)?;
        let body = non_blank("message", input.message)?;
        let to = recipient(ctx, &input.recipient).await?;
        // NOTE: the target is always this account's own message. Taking an
        // author from the caller would let an edit aim at someone else's message.
        let me = ctx.signal.account().await?.service_id;
        let target = MessageId {
            author: me,
            sent_at_ms: input.target_timestamp.0,
        };
        let edit = Outgoing::Edit {
            target,
            body: body.clone(),
        };
        ctx.signal.send(to, edit).await?;
        ctx.store
            .apply_edit(target, me, &body)
            .await
            .context("Signal accepted the edit, but the stored copy wasn't updated")?;
        Ok(json!({ "status": "edited", "target_timestamp": target.sent_at_ms }))
    }
}

#[cfg(test)]
mod tests {
    use relay_signal::Outgoing;
    use serde_json::json;

    use crate::{Registry, testing::*};
    use relay_core::prelude::*;

    /// A reaction must name the target by its author and timestamp and go to
    /// the conversation given, or it lands on the wrong message or nowhere.
    #[tokio::test]
    async fn react_to_message_sends_the_reaction_to_the_target_message() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        Registry::standard()
            .call(
                &ctx,
                "react_to_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "target_author": sid(3).to_string(),
                    "target_timestamp": 42,
                    "emoji": "👍",
                }),
            )
            .await
            .unwrap();
        let target = MessageId {
            author: sid(3),
            sent_at_ms: 42,
        };
        assert_eq!(
            signal.sent(),
            [(
                Recipient::Contact(sid(2)),
                Outgoing::Reaction {
                    target,
                    emoji: "👍".into(),
                    remove: false,
                }
            )]
        );
    }

    /// A message this account sent into `conversation` at `sent_at_ms`.
    fn mine(conversation: Recipient, sent_at_ms: u64, body: &str) -> Message {
        Message {
            id: MessageId {
                author: sid(ME),
                sent_at_ms,
            },
            conversation,
            sender: sid(ME),
            from_me: true,
            ..incoming(ME, sent_at_ms, body)
        }
    }

    /// The edit goes to Signal as this account's own message, and only then
    /// replaces the stored body and flags it edited.
    #[tokio::test]
    async fn edit_message_sends_the_edit_then_updates_the_stored_copy() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let dm = Recipient::Contact(sid(2));
        ctx.store.insert(&mine(dm, 42, "teh cat")).await.unwrap();
        Registry::standard()
            .call(
                &ctx,
                "edit_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "target_timestamp": 42,
                    "message": "the cat",
                }),
            )
            .await
            .unwrap();
        let target = MessageId {
            author: sid(ME),
            sent_at_ms: 42,
        };
        assert_eq!(
            signal.sent(),
            [(
                dm,
                Outgoing::Edit {
                    target,
                    body: "the cat".into(),
                }
            )]
        );
        let stored = ctx.store.history(dm, None, 10).await.unwrap();
        assert_eq!(stored[0].body.as_deref(), Some("the cat"), "{stored:?}");
        assert!(stored[0].edited, "edit not flagged: {stored:?}");
    }

    /// Someone else's message at the same timestamp must survive an edit: the
    /// tool only ever targets this account's own messages, so the store's
    /// author check never gets a foreign target to accept.
    #[tokio::test]
    async fn edit_message_cannot_rewrite_someone_elses_message() {
        let (ctx, _) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store
            .insert(&incoming(2, 42, "their words"))
            .await
            .unwrap();
        let result = Registry::standard()
            .call(
                &ctx,
                "edit_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "target_timestamp": 42,
                    "message": "my words",
                }),
            )
            .await;
        let stored = ctx
            .store
            .history(Recipient::Contact(sid(2)), None, 10)
            .await
            .unwrap();
        assert_eq!(stored[0].body.as_deref(), Some("their words"), "{result:?}");
        assert!(
            !stored[0].edited,
            "foreign message flagged edited: {stored:?}"
        );
    }

    /// If Signal refuses the edit, the stored copy must keep the old text, or
    /// the local history shows an edit nobody else ever saw.
    #[tokio::test]
    async fn edit_message_leaves_the_store_alone_when_signal_refuses() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let left = Recipient::Group(GroupId::new([7; 32]));
        ctx.store.insert(&mine(left, 42, "teh cat")).await.unwrap();
        let result = Registry::standard()
            .call(
                &ctx,
                "edit_message",
                json!({
                    "recipient": GroupId::new([7; 32]).to_string(),
                    "target_timestamp": 42,
                    "message": "the cat",
                }),
            )
            .await;
        assert!(
            result.is_err(),
            "edit to a group we left succeeded: {result:?}"
        );
        assert!(signal.sent().is_empty(), "refused send recorded");
        let stored = ctx.store.history(left, None, 10).await.unwrap();
        assert_eq!(stored[0].body.as_deref(), Some("teh cat"), "{stored:?}");
        assert!(!stored[0].edited, "refused edit flagged: {stored:?}");
    }
}
