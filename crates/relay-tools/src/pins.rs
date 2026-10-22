//! Pinning a message to the top of a conversation for everyone in it. Signal
//! names the target by author and send timestamp, as for reactions.

use std::num::NonZeroU32;

use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::Outgoing;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{Timestamp, contact, contact_schema, parse, recipient, recipient_schema},
    mutate::check_target,
};

pub struct PinMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PinInput {
    recipient: String,
    target_author: String,
    target_timestamp: Timestamp,
    duration_secs: Option<PinDuration>,
}

#[async_trait]
impl Tool for PinMessage {
    fn name(&self) -> &'static str {
        "pin_message"
    }
    fn description(&self) -> &'static str {
        "Pin a message to the top of a DM or group for everyone in it. recipient is the \
         conversation (as list_conversations names it, group ids included); target_author and \
         target_timestamp name the message, as get_conversation returns it, and it must be \
         stored locally in that conversation. duration_secs unpins it automatically after that \
         many seconds (up to 30 days); without it the pin stays until unpin_message. Everyone \
         in the chat sees the pin notice. Sends to Signal; nothing is stored locally."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "target_author": contact_schema("Who wrote the message"),
                "target_timestamp": Timestamp::schema("The message to pin"),
                "duration_secs": PinDuration::schema(),
            },
            "required": ["recipient", "target_author", "target_timestamp"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: PinInput = parse(input)?;
        let to = recipient(ctx, &input.recipient).await?;
        let target = MessageId {
            author: contact(ctx, &input.target_author).await?,
            sent_at_ms: input.target_timestamp.ms(),
        };
        check_target(ctx, to, target).await?;
        let duration_secs = input.duration_secs.map(|d| d.0);
        ctx.signal
            .send(
                to,
                Outgoing::Pin {
                    target,
                    duration_secs,
                },
            )
            .await?;
        Ok(json!({ "status": "pinned" }))
    }
}

pub struct UnpinMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnpinInput {
    recipient: String,
    target_author: String,
    target_timestamp: Timestamp,
}

#[async_trait]
impl Tool for UnpinMessage {
    fn name(&self) -> &'static str {
        "unpin_message"
    }
    fn description(&self) -> &'static str {
        "Remove a pin from a message in a DM or group, for everyone in it. recipient is the \
         conversation; target_author and target_timestamp name the pinned message, as \
         get_conversation returns it, and it must be stored locally in that conversation. \
         Sends to Signal; nothing is stored locally."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "target_author": contact_schema("Who wrote the message"),
                "target_timestamp": Timestamp::schema("The message to unpin"),
            },
            "required": ["recipient", "target_author", "target_timestamp"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: UnpinInput = parse(input)?;
        let to = recipient(ctx, &input.recipient).await?;
        let target = MessageId {
            author: contact(ctx, &input.target_author).await?,
            sent_at_ms: input.target_timestamp.ms(),
        };
        check_target(ctx, to, target).await?;
        ctx.signal.send(to, Outgoing::Unpin { target }).await?;
        Ok(json!({ "status": "unpinned" }))
    }
}

/// How long a pin lasts: 1 second to 30 days.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "u64")]
struct PinDuration(NonZeroU32);

impl PinDuration {
    const MAX: u32 = 30 * 24 * 60 * 60;

    fn schema() -> Value {
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": Self::MAX,
            "description": "Seconds until the pin lapses, 1 to 2592000 (30 days); omit to pin until unpinned",
        })
    }
}

impl TryFrom<u64> for PinDuration {
    type Error = String;
    fn try_from(n: u64) -> Result<Self, String> {
        u32::try_from(n)
            .ok()
            .filter(|n| *n <= Self::MAX)
            .and_then(NonZeroU32::new)
            .map(Self)
            .ok_or_else(|| format!("duration_secs must be 1 to {}, got {n}", Self::MAX))
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use relay_signal::Outgoing;
    use serde_json::json;

    use crate::{Registry, ToolError, testing::*};
    use relay_core::prelude::*;

    /// A pin must name the stored target by author and timestamp and go to
    /// that conversation with the duration asked for, or it pins the wrong
    /// message, or pins forever when the caller asked for a day.
    #[tokio::test]
    async fn pin_message_pins_the_target_in_its_conversation_for_the_duration_given() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store.insert(&incoming(2, 42, "hi")).await.unwrap();
        Registry::standard()
            .call(
                &ctx,
                "pin_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "target_author": sid(2).to_string(),
                    "target_timestamp": 42,
                    "duration_secs": 86400,
                }),
            )
            .await
            .unwrap();
        let target = MessageId {
            author: sid(2),
            sent_at_ms: 42,
        };
        assert_eq!(
            signal.sent(),
            [(
                Recipient::Contact(sid(2)),
                Outgoing::Pin {
                    target,
                    duration_secs: NonZeroU32::new(86400),
                }
            )]
        );
    }

    /// An unpin must name the same target a pin did, in the same chat, or the
    /// pin stays up for everyone.
    #[tokio::test]
    async fn unpin_message_unpins_the_target_in_its_conversation() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store.insert(&incoming(2, 42, "hi")).await.unwrap();
        Registry::standard()
            .call(
                &ctx,
                "unpin_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "target_author": sid(2).to_string(),
                    "target_timestamp": 42,
                }),
            )
            .await
            .unwrap();
        let target = MessageId {
            author: sid(2),
            sent_at_ms: 42,
        };
        assert_eq!(
            signal.sent(),
            [(Recipient::Contact(sid(2)), Outgoing::Unpin { target })]
        );
    }

    /// A pin naming a message from another conversation must be refused
    /// before sending, or it reveals that message to a chat that never saw it.
    #[tokio::test]
    async fn a_pin_naming_a_message_from_another_conversation_is_refused_and_nothing_is_sent() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store
            .insert(&incoming(3, 42, "to me only"))
            .await
            .unwrap();
        let result = Registry::standard()
            .call(
                &ctx,
                "pin_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "target_author": sid(3).to_string(),
                    "target_timestamp": 42,
                }),
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "got {result:?}"
        );
        assert!(signal.sent().is_empty(), "sent {:?}", signal.sent());
    }
}
