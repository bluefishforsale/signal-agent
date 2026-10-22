//! Tools that put a new message, typing indicator or receipt on the wire.

use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::Outgoing;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{Timestamp, contact, contact_schema, group, group_schema, non_blank, parse},
};

pub struct SendMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendMessageInput {
    recipient: String,
    message: String,
    quote_author: Option<String>,
    quote_timestamp: Option<Timestamp>,
}

#[async_trait]
impl Tool for SendMessage {
    fn name(&self) -> &'static str {
        "send_message"
    }
    fn description(&self) -> &'static str {
        "Send a text message to one Signal contact. Use send_group_message for groups and \
         send_note_to_self for your own Note to Self. recipient is the contact's phone number \
         or service id; message is the text. To reply, quote_author and quote_timestamp \
         name the quoted message (its id's author and sent_at_ms). Not idempotent: \
         repeating sends a duplicate. The sent message is saved to the local store. Returns \
         the new message's id, which replies, reactions, edits and deletes reference."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": contact_schema("Who to send to"),
                "message": {"type": "string", "minLength": 1, "description": "Message text to send"},
                "quote_author": contact_schema("Author of the message being replied to"),
                "quote_timestamp": Timestamp::schema("Send time of the message being replied to"),
            },
            "required": ["recipient", "message"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SendMessageInput = parse(input)?;
        let to = Recipient::Contact(contact(ctx, &input.recipient).await?);
        let body = non_blank("message", input.message)?;
        let quote = quote(ctx, input.quote_author, input.quote_timestamp).await?;
        send_text(ctx, to, body, quote).await
    }
}

pub struct SendGroupMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendGroupMessageInput {
    group_id: String,
    message: String,
    quote_author: Option<String>,
    quote_timestamp: Option<Timestamp>,
}

#[async_trait]
impl Tool for SendGroupMessage {
    fn name(&self) -> &'static str {
        "send_group_message"
    }
    fn description(&self) -> &'static str {
        "Send a text message to a Signal group you belong to. Use send_message for one \
         contact. group_id comes from list_conversations; message is the text. To reply, \
         quote_author and quote_timestamp name the quoted message (its id's author and \
         sent_at_ms). Not idempotent: repeating sends a duplicate. The sent message is saved \
         to the local store. Returns the new message's id, which replies, reactions, edits \
         and deletes reference."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "group_id": group_schema(),
                "message": {"type": "string", "minLength": 1, "description": "Message text to send"},
                "quote_author": contact_schema("Author of the message being replied to"),
                "quote_timestamp": Timestamp::schema("Send time of the message being replied to"),
            },
            "required": ["group_id", "message"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SendGroupMessageInput = parse(input)?;
        let to = Recipient::Group(group(&input.group_id)?);
        let body = non_blank("message", input.message)?;
        let quote = quote(ctx, input.quote_author, input.quote_timestamp).await?;
        send_text(ctx, to, body, quote).await
    }
}

/// The quoted message named by an author and send time, which must come
/// together or not at all.
async fn quote(
    ctx: &ToolContext,
    author: Option<String>,
    timestamp: Option<Timestamp>,
) -> Result<Option<MessageId>, ToolError> {
    match (author, timestamp) {
        (None, None) => Ok(None),
        (Some(author), Some(timestamp)) => Ok(Some(MessageId {
            author: contact(ctx, &author).await?,
            sent_at_ms: timestamp.ms(),
        })),
        _ => Err(ToolError::InvalidInput(
            "quote_author and quote_timestamp go together; give both or neither".into(),
        )),
    }
}

/// Sends `body` to `to` and records it in the local history as this
/// account's own message.
// NOTE: Signal doesn't echo a device's own sends back to it, so a send that
// skips the store never shows up in get_conversation or search_messages.
async fn send_text(
    ctx: &ToolContext,
    to: Recipient,
    body: String,
    quote: Option<MessageId>,
) -> Result<Value, ToolError> {
    let id = ctx
        .signal
        .send(
            to,
            Outgoing::Text {
                body: body.clone(),
                quote,
            },
        )
        .await?;
    ctx.store
        .insert(&Message {
            id,
            conversation: to,
            sender: id.author,
            from_me: true,
            received_at_ms: id.sent_at_ms,
            body: Some(body),
            attachments: vec![],
            quote,
            edited: false,
            deleted: false,
        })
        .await?;
    Ok(json!({ "id": id }))
}

#[cfg(test)]
mod tests {
    use relay_core::prelude::*;
    use relay_signal::Outgoing;
    use serde_json::json;

    use std::sync::Arc;

    use relay_signal::{Account, FakeSignal, Group};
    use relay_store::MemoryStore;

    use crate::{Registry, ToolContext, ToolError, testing::*};

    const GROUP: [u8; 32] = [7; 32];

    /// A full-access linked profile that belongs to one group, GROUP.
    fn group_ctx() -> (ToolContext, Arc<FakeSignal>) {
        let signal = Arc::new(
            FakeSignal::new(Account {
                service_id: sid(ME),
                phone_number: Some("+15550000001".into()),
                kind: ProfileKind::Linked,
                device_name: "test".into(),
            })
            .with_groups(vec![Group {
                id: GroupId::new(GROUP),
                title: "band".into(),
                members: vec![sid(ME), sid(2)],
            }]),
        );
        let ctx = ToolContext {
            store: Arc::new(MemoryStore::default()),
            signal: signal.clone(),
            access: AccessMode::Full,
            kind: ProfileKind::Linked,
        };
        (ctx, signal)
    }

    /// A group send goes to the group, not to any one member, and is stored
    /// in the group's conversation with a reply quote intact.
    #[tokio::test]
    async fn send_group_message_sends_to_the_group_and_stores_it_there() {
        let (ctx, signal) = group_ctx();
        let group = GroupId::new(GROUP);
        let out = Registry::standard()
            .call(
                &ctx,
                "send_group_message",
                json!({
                    "group_id": group.to_string(),
                    "message": "soundcheck at 6",
                    "quote_author": sid(2).to_string(),
                    "quote_timestamp": 99,
                }),
            )
            .await
            .unwrap();
        let quoted = MessageId {
            author: sid(2),
            sent_at_ms: 99,
        };
        let to = Recipient::Group(group);
        let text = Outgoing::Text {
            body: "soundcheck at 6".into(),
            quote: Some(quoted),
        };
        assert_eq!(signal.sent(), [(to, text)]);
        let id: MessageId = serde_json::from_value(out["id"].clone()).unwrap();
        let stored = ctx.store.history(to, None, 10).await.unwrap();
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert_eq!((stored[0].id, stored[0].quote), (id, Some(quoted)));
    }

    /// A send must reach Signal as plain text to that one contact, return the
    /// id clients quote, edit or delete it by, and land in the local history,
    /// since Signal never echoes this device's own sends back to it.
    #[tokio::test]
    async fn send_message_sends_text_returns_its_id_and_stores_it_as_mine() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let out = Registry::standard()
            .call(
                &ctx,
                "send_message",
                json!({"recipient": sid(2).to_string(), "message": "hi"}),
            )
            .await
            .unwrap();
        let to = Recipient::Contact(sid(2));
        let text = Outgoing::Text {
            body: "hi".into(),
            quote: None,
        };
        assert_eq!(signal.sent(), [(to, text)]);
        let id: MessageId = serde_json::from_value(out["id"].clone()).unwrap();
        assert_eq!(id.author, sid(ME), "{out}");
        let stored = ctx.store.history(to, None, 10).await.unwrap();
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert_eq!(stored[0].id, id);
        assert!(stored[0].from_me, "{stored:?}");
        assert_eq!(stored[0].body.as_deref(), Some("hi"));
    }

    /// A reply must carry the quoted message's id to Signal and keep it in
    /// the stored copy, or the thread breaks on both ends.
    #[tokio::test]
    async fn send_message_quotes_the_message_named_by_author_and_timestamp() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        Registry::standard()
            .call(
                &ctx,
                "send_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "message": "agreed",
                    "quote_author": sid(2).to_string(),
                    "quote_timestamp": 1234,
                }),
            )
            .await
            .unwrap();
        let quoted = MessageId {
            author: sid(2),
            sent_at_ms: 1234,
        };
        let to = Recipient::Contact(sid(2));
        let text = Outgoing::Text {
            body: "agreed".into(),
            quote: Some(quoted),
        };
        assert_eq!(signal.sent(), [(to, text)]);
        let stored = ctx.store.history(to, None, 10).await.unwrap();
        assert_eq!(stored[0].quote, Some(quoted), "{stored:?}");
    }

    /// Half a quote can't name a message; sending it as a plain message would
    /// silently drop the reply the caller asked for.
    #[tokio::test]
    async fn send_message_refuses_half_a_quote_without_sending() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let registry = Registry::standard();
        for half in [
            json!({"quote_author": sid(2).to_string()}),
            json!({"quote_timestamp": 1234}),
        ] {
            let mut input = json!({"recipient": sid(2).to_string(), "message": "hi"});
            input
                .as_object_mut()
                .unwrap()
                .extend(half.as_object().unwrap().clone());
            let result = registry.call(&ctx, "send_message", input.clone()).await;
            assert!(
                matches!(result, Err(ToolError::InvalidInput(_))),
                "{input} gave {result:?}"
            );
        }
        assert!(signal.sent().is_empty(), "{:?}", signal.sent());
    }
}
