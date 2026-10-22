//! Tools that act on a message already in a conversation: reactions, edits and
//! deletes. Signal names the target by author and send timestamp, and the local
//! store follows only once Signal has accepted the change.

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
}
