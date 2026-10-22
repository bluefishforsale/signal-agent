//! Polls: creating one, voting on one, and closing your own. Signal names a
//! poll by its author and send timestamp, like any other message.

use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::Outgoing;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{non_blank, parse, recipient, recipient_schema},
};

/// Signal's apps offer at most this many options on a poll.
const MAX_OPTIONS: usize = 10;

pub struct CreatePoll;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePollInput {
    recipient: String,
    question: String,
    options: Vec<String>,
    #[serde(default)]
    multi_select: bool,
}

#[async_trait]
impl Tool for CreatePoll {
    fn name(&self) -> &'static str {
        "create_poll"
    }
    fn description(&self) -> &'static str {
        "Send a poll to a DM or group. recipient is the conversation (as list_conversations \
         names it, group ids included); question is the prompt; options are 2 to 10 answers, \
         numbered from 0 in the order given, which is how vote_poll names them. multi_select \
         lets people pick more than one (default false). Not idempotent: repeating sends a \
         second poll. The poll is saved to the local store. Returns its message id, which \
         vote_poll and terminate_poll reference."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "question": {"type": "string", "minLength": 1, "description": "The poll's question"},
                "options": {
                    "type": "array",
                    "items": {"type": "string", "minLength": 1},
                    "minItems": 2,
                    "maxItems": MAX_OPTIONS,
                    "description": "The answers to choose from, in display order",
                },
                "multi_select": {
                    "type": "boolean",
                    "default": false,
                    "description": "Allow choosing more than one option (default false)",
                },
            },
            "required": ["recipient", "question", "options"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: CreatePollInput = parse(input)?;
        let question = non_blank("question", input.question)?;
        if !(2..=MAX_OPTIONS).contains(&input.options.len()) {
            return Err(ToolError::InvalidInput(format!(
                "options must hold 2 to {MAX_OPTIONS} answers, got {}",
                input.options.len()
            )));
        }
        let options = input
            .options
            .into_iter()
            .map(|o| non_blank("each option", o))
            .collect::<Result<Vec<_>, _>>()?;
        let to = recipient(ctx, &input.recipient).await?;
        let poll = Outgoing::PollCreate {
            question: question.clone(),
            options,
            multi_select: input.multi_select,
        };
        let id = ctx.signal.send(to, poll).await?;
        ctx.store
            .insert(&Message {
                id,
                conversation: to,
                sender: id.author,
                from_me: true,
                received_at_ms: id.sent_at_ms,
                body: Some(question),
                attachments: vec![],
                quote: None,
                edited: false,
                deleted: false,
            })
            .await?;
        Ok(json!({ "id": id }))
    }
}

#[cfg(test)]
mod tests {
    use relay_signal::Outgoing;
    use serde_json::json;

    use crate::{Registry, testing::*};
    use relay_core::prelude::*;

    /// A poll must reach the conversation with its options in order, and be
    /// stored, or votes and terminate_poll can't name it afterwards.
    #[tokio::test]
    async fn create_poll_sends_the_options_in_order_and_stores_the_poll() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let out = Registry::standard()
            .call(
                &ctx,
                "create_poll",
                json!({
                    "recipient": sid(2).to_string(),
                    "question": "Lunch?",
                    "options": ["tacos", "pho"],
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            signal.sent(),
            [(
                Recipient::Contact(sid(2)),
                Outgoing::PollCreate {
                    question: "Lunch?".into(),
                    options: vec!["tacos".into(), "pho".into()],
                    multi_select: false,
                }
            )]
        );
        let id: MessageId = serde_json::from_value(out["id"].clone()).unwrap();
        let stored = ctx.store.get(id).await.unwrap();
        assert!(
            stored.is_some_and(|m| m.conversation == Recipient::Contact(sid(2))),
            "poll {id:?} not stored in its conversation"
        );
    }
}
