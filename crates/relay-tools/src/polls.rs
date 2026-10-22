//! Polls: creating one, voting on one, and closing your own. Signal names a
//! poll by its author and send timestamp, like any other message.

use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::Outgoing;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{Timestamp, contact, contact_schema, non_blank, parse, recipient, recipient_schema},
    mutate::check_target,
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

pub struct VotePoll;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VotePollInput {
    recipient: String,
    poll_author: String,
    poll_timestamp: Timestamp,
    option_indexes: Vec<OptionIndex>,
}

#[async_trait]
impl Tool for VotePoll {
    fn name(&self) -> &'static str {
        "vote_poll"
    }
    fn description(&self) -> &'static str {
        "Vote on a poll in a DM or group. recipient is the conversation; poll_author and \
         poll_timestamp name the poll's message, as get_conversation returns it, and it must be \
         stored locally in that conversation. option_indexes are the chosen options, numbered \
         from 0 in the poll's order; pick one unless the poll allows several. Voting again \
         replaces your earlier vote. Sends to Signal; nothing is stored locally."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "poll_author": contact_schema("Who created the poll"),
                "poll_timestamp": Timestamp::schema("The poll to vote on"),
                "option_indexes": {
                    "type": "array",
                    "items": OptionIndex::schema(),
                    "minItems": 1,
                    "maxItems": MAX_OPTIONS,
                    "description": "The chosen options, numbered from 0, without repeats",
                },
            },
            "required": ["recipient", "poll_author", "poll_timestamp", "option_indexes"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: VotePollInput = parse(input)?;
        let mut chosen: Vec<u32> = input.option_indexes.iter().map(|i| i.0).collect();
        chosen.sort_unstable();
        chosen.dedup();
        if chosen.is_empty() || chosen.len() != input.option_indexes.len() {
            return Err(ToolError::InvalidInput(format!(
                "option_indexes must name 1 to {MAX_OPTIONS} different options"
            )));
        }
        let to = recipient(ctx, &input.recipient).await?;
        let poll = MessageId {
            author: contact(ctx, &input.poll_author).await?,
            sent_at_ms: input.poll_timestamp.ms(),
        };
        check_target(ctx, to, poll).await?;
        let vote_count = ctx.store.next_poll_vote(poll).await?;
        let vote = Outgoing::PollVote {
            poll,
            option_indexes: input.option_indexes.iter().map(|i| i.0).collect(),
            vote_count,
        };
        ctx.signal.send(to, vote).await?;
        Ok(json!({ "status": "voted", "vote_count": vote_count }))
    }
}

/// A poll option's position, from 0.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "u64")]
struct OptionIndex(u32);

impl OptionIndex {
    const MAX: u32 = MAX_OPTIONS as u32 - 1;

    fn schema() -> Value {
        json!({
            "type": "integer",
            "minimum": 0,
            "maximum": Self::MAX,
            "description": "An option's position in the poll, from 0",
        })
    }
}

impl TryFrom<u64> for OptionIndex {
    type Error = String;
    fn try_from(n: u64) -> Result<Self, String> {
        match u32::try_from(n) {
            Ok(n @ 0..=Self::MAX) => Ok(Self(n)),
            _ => Err(format!(
                "option indexes must be 0 to {}, got {n}",
                Self::MAX
            )),
        }
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

    /// Signal's apps keep only the vote with the highest count, so a changed
    /// vote must carry a higher count than the first or it's ignored.
    #[tokio::test]
    async fn a_second_vote_on_a_poll_carries_a_higher_vote_count() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store.insert(&incoming(2, 42, "Lunch?")).await.unwrap();
        let registry = Registry::standard();
        for choice in [0, 1] {
            registry
                .call(
                    &ctx,
                    "vote_poll",
                    json!({
                        "recipient": sid(2).to_string(),
                        "poll_author": sid(2).to_string(),
                        "poll_timestamp": 42,
                        "option_indexes": [choice],
                    }),
                )
                .await
                .unwrap();
        }
        let poll = MessageId {
            author: sid(2),
            sent_at_ms: 42,
        };
        let sent = signal.sent();
        let counts: Vec<u32> = sent
            .iter()
            .map(|(to, out)| match out {
                Outgoing::PollVote {
                    poll: p,
                    vote_count,
                    ..
                } if *p == poll && *to == Recipient::Contact(sid(2)) => *vote_count,
                other => panic!("unexpected send {other:?}"),
            })
            .collect();
        assert!(
            counts.len() == 2 && counts[1] > counts[0],
            "vote counts {counts:?}"
        );
    }
}
