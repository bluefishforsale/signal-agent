//! Tools that put a new message, typing indicator or receipt on the wire.

use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::Outgoing;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{
        Timestamp, contact, contact_schema, group, group_schema, non_blank, parse, recipient,
        recipient_schema,
    },
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
        send_text(ctx, to, body, quote, vec![]).await
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
        send_text(ctx, to, body, quote, vec![]).await
    }
}

pub struct SendNoteToSelf;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendNoteToSelfInput {
    message: String,
    quote_timestamp: Option<Timestamp>,
}

#[async_trait]
impl Tool for SendNoteToSelf {
    fn name(&self) -> &'static str {
        "send_note_to_self"
    }
    fn description(&self) -> &'static str {
        "Send a message to your own Note to Self chat, which syncs to all your Signal \
         devices. Use for reminders, bookmarks or drafts; use send_message to message anyone \
         else. message is the text. To thread a follow-up under an earlier note, pass \
         quote_timestamp (that note's sent_at_ms). Not idempotent. Saved to the local store. \
         Returns the new message's id."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {"type": "string", "minLength": 1, "description": "Note text to save"},
                "quote_timestamp": Timestamp::schema("Send time of the note being followed up on"),
            },
            "required": ["message"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SendNoteToSelfInput = parse(input)?;
        let body = non_blank("message", input.message)?;
        let me = ctx.signal.account().await?.service_id;
        let quote = input.quote_timestamp.map(|t| MessageId {
            author: me,
            sent_at_ms: t.ms(),
        });
        send_text(ctx, Recipient::Contact(me), body, quote, vec![]).await
    }
}

pub struct SetTyping;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetTypingInput {
    recipient: String,
    #[serde(default)]
    stop: bool,
}

#[async_trait]
impl Tool for SetTyping {
    fn name(&self) -> &'static str {
        "set_typing"
    }
    fn description(&self) -> &'static str {
        "Show or cancel the typing indicator in a DM or group. Purely cosmetic; use before \
         send_message or send_group_message in an automated reply. recipient is a contact or \
         group as list_conversations names it. stop=true cancels an active indicator \
         (default false starts one). It expires by itself after about 15 seconds and clears \
         when you send, so one call per message is enough. Nothing is stored."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "stop": {"type": "boolean", "default": false, "description": "True cancels the indicator instead of starting it"},
            },
            "required": ["recipient"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SetTypingInput = parse(input)?;
        let to = recipient(ctx, &input.recipient).await?;
        let started = !input.stop;
        ctx.signal.send(to, Outgoing::Typing { started }).await?;
        Ok(json!({ "status": if started { "typing" } else { "stopped" } }))
    }
}

pub struct SendReadReceipt;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendReadReceiptInput {
    sender: String,
    timestamps: Vec<Timestamp>,
}

impl SendReadReceipt {
    const MAX_TIMESTAMPS: usize = 500;
}

#[async_trait]
impl Tool for SendReadReceipt {
    fn name(&self) -> &'static str {
        "send_read_receipt"
    }
    fn description(&self) -> &'static str {
        "Send a read receipt so a contact's Signal shows their messages as Read, and mark \
         those messages read in the local store. Not for groups. sender is the contact who \
         wrote the messages; timestamps are their sent_at_ms values (1 to 500, batched in \
         one call). Only shown if the sender has read receipts on. Repeating is harmless."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sender": contact_schema("Who wrote the messages being acknowledged"),
                "timestamps": {
                    "type": "array",
                    "items": Timestamp::schema("Send time of a message being acknowledged"),
                    "minItems": 1,
                    "maxItems": Self::MAX_TIMESTAMPS,
                    "description": "Send times of the messages to acknowledge",
                },
            },
            "required": ["sender", "timestamps"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SendReadReceiptInput = parse(input)?;
        if !(1..=Self::MAX_TIMESTAMPS).contains(&input.timestamps.len()) {
            return Err(ToolError::InvalidInput(format!(
                "timestamps must hold 1 to {} entries, got {}",
                Self::MAX_TIMESTAMPS,
                input.timestamps.len()
            )));
        }
        let sender = contact(ctx, &input.sender).await?;
        let targets: Vec<_> = input
            .timestamps
            .iter()
            .map(|t| MessageId {
                author: sender,
                sent_at_ms: t.ms(),
            })
            .collect();
        ctx.signal
            .send(
                Recipient::Contact(sender),
                Outgoing::ReadReceipt {
                    targets: targets.clone(),
                },
            )
            .await?;
        ctx.store.mark_read(&targets).await?;
        Ok(json!({ "status": "read receipt sent" }))
    }
}

/// The quoted message named by an author and send time, which must come
/// together or not at all.
pub(crate) async fn quote(
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

/// Sends `body` with `attachments` to `to` and records it in the local
/// history as this account's own message.
// NOTE: Signal doesn't echo a device's own sends back to it, so a send that
// skips the store never shows up in get_conversation or search_messages.
pub(crate) async fn send_text(
    ctx: &ToolContext,
    to: Recipient,
    body: String,
    quote: Option<MessageId>,
    attachments: Vec<AttachmentRef>,
) -> Result<Value, ToolError> {
    let id = ctx
        .signal
        .send(
            to,
            Outgoing::Text {
                body: body.clone(),
                quote,
                attachments: attachments.clone(),
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
            body: (!body.is_empty()).then_some(body),
            attachments,
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

    use std::{sync::Arc, time::Duration};

    use relay_signal::{Account, FakeSignal, Group, RateLimited};
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
                admins: vec![sid(ME)],
            }]),
        );
        let ctx = ToolContext {
            store: Arc::new(MemoryStore::default()),
            signal: signal.clone(),
            access: AccessMode::Full,
            kind: ProfileKind::Linked,
            send_roots: Arc::default(),
            webhook: None,
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
            attachments: vec![],
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
            attachments: vec![],
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
            attachments: vec![],
        };
        assert_eq!(signal.sent(), [(to, text)]);
        let stored = ctx.store.history(to, None, 10).await.unwrap();
        assert_eq!(stored[0].quote, Some(quoted), "{stored:?}");
    }

    /// Note to Self is the DM with your own account; it must go there and be
    /// stored there, never to a contact.
    #[tokio::test]
    async fn send_note_to_self_sends_to_the_accounts_own_conversation() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let out = Registry::standard()
            .call(
                &ctx,
                "send_note_to_self",
                json!({"message": "buy cat food"}),
            )
            .await
            .unwrap();
        let me = Recipient::Contact(sid(ME));
        let text = Outgoing::Text {
            body: "buy cat food".into(),
            quote: None,
            attachments: vec![],
        };
        assert_eq!(signal.sent(), [(me, text)]);
        let id: MessageId = serde_json::from_value(out["id"].clone()).unwrap();
        let stored = ctx.store.history(me, None, 10).await.unwrap();
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert_eq!(stored[0].id, id);
    }

    /// Every note is the account's own, so a follow-up names the earlier note
    /// by send time alone and the quote's author must be this account.
    #[tokio::test]
    async fn send_note_to_self_threads_under_an_earlier_note_by_timestamp() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        Registry::standard()
            .call(
                &ctx,
                "send_note_to_self",
                json!({"message": "and litter", "quote_timestamp": 42}),
            )
            .await
            .unwrap();
        let earlier = MessageId {
            author: sid(ME),
            sent_at_ms: 42,
        };
        let text = Outgoing::Text {
            body: "and litter".into(),
            quote: Some(earlier),
            attachments: vec![],
        };
        assert_eq!(signal.sent(), [(Recipient::Contact(sid(ME)), text)]);
    }

    /// Typing starts by default and stop cancels it, in a DM or a group; it is
    /// cosmetic, so nothing lands in the local history.
    #[tokio::test]
    async fn set_typing_starts_or_stops_in_a_dm_or_group_and_stores_nothing() {
        let (ctx, signal) = group_ctx();
        let registry = Registry::standard();
        let group = GroupId::new(GROUP);
        registry
            .call(&ctx, "set_typing", json!({"recipient": sid(2).to_string()}))
            .await
            .unwrap();
        registry
            .call(
                &ctx,
                "set_typing",
                json!({"recipient": group.to_string(), "stop": true}),
            )
            .await
            .unwrap();
        assert_eq!(
            signal.sent(),
            [
                (
                    Recipient::Contact(sid(2)),
                    Outgoing::Typing { started: true }
                ),
                (Recipient::Group(group), Outgoing::Typing { started: false }),
            ]
        );
        let stats = ctx.store.stats().await.unwrap();
        assert_eq!(stats.messages, 0, "{stats:?}");
    }

    /// One receipt covers every timestamp, each naming the sender's message,
    /// and those messages stop counting as unread locally too.
    #[tokio::test]
    async fn send_read_receipt_acknowledges_the_senders_messages_and_marks_them_read() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        for (t, body) in [(100, "a"), (200, "b"), (300, "c")] {
            ctx.store.insert(&incoming(2, t, body)).await.unwrap();
        }
        Registry::standard()
            .call(
                &ctx,
                "send_read_receipt",
                json!({"sender": sid(2).to_string(), "timestamps": [100, 200]}),
            )
            .await
            .unwrap();
        let targets = [100, 200].map(|sent_at_ms| MessageId {
            author: sid(2),
            sent_at_ms,
        });
        let receipt = Outgoing::ReadReceipt {
            targets: targets.to_vec(),
        };
        assert_eq!(signal.sent(), [(Recipient::Contact(sid(2)), receipt)]);
        let unread: Vec<_> = ctx.store.unread().await.unwrap();
        assert_eq!(unread.len(), 1, "{unread:?}");
        assert_eq!(unread[0].id.sent_at_ms, 300);
    }

    /// A send Signal refused must not show up in the local history as if it
    /// went out.
    #[tokio::test]
    async fn a_failed_send_stores_nothing() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let result = Registry::standard()
            .call(
                &ctx,
                "send_group_message",
                json!({"group_id": GroupId::new(GROUP).to_string(), "message": "hi"}),
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::Internal(_))),
            "got {result:?}"
        );
        assert!(signal.sent().is_empty(), "{:?}", signal.sent());
        let stats = ctx.store.stats().await.unwrap();
        assert_eq!(stats.messages, 0, "{stats:?}");
    }

    /// Signal rate-limiting a send must reach the MCP client as a clear error
    /// with Signal's wait, after exactly one attempt: a tool that retried, or
    /// stored the message as sent, would keep an AI client hammering Signal.
    #[tokio::test]
    async fn a_rate_limited_send_reports_signals_wait_after_one_attempt() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        signal.rate_limit_next_send(RateLimited {
            message: "Rate limit exceeded: retry after 30s".into(),
            retry_after: Some(Duration::from_secs(30)),
        });
        let result = Registry::standard()
            .call(
                &ctx,
                "send_message",
                json!({"recipient": sid(2).to_string(), "message": "hi"}),
            )
            .await;
        let Err(ToolError::Internal(e)) = &result else {
            panic!("expected a refusal, got {result:?}");
        };
        assert!(e.downcast_ref::<RateLimited>().is_some(), "untyped: {e:#}");
        let shown = result.unwrap_err().to_string();
        assert!(
            shown.contains("rate-limited") && shown.contains("30s"),
            "client would see {shown:?}"
        );
        assert_eq!(signal.attempts(), 1, "{:?}", signal.sent());
        let stats = ctx.store.stats().await.unwrap();
        assert_eq!(stats.messages, 0, "{stats:?}");
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
