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
    input::{
        Timestamp, contact, contact_schema, group, group_schema, non_blank, parse, recipient,
        recipient_schema,
    },
};

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
         get_conversation returns it; the message must be in the local store, in that \
         conversation. You have one reaction per message: a new emoji replaces \
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
            sent_at_ms: input.target_timestamp.ms(),
        };
        check_target(ctx, to, target).await?;
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
         your message, as get_conversation or a send result returns it, and it must be stored \
         locally in that conversation; message is the new full text. Only your own messages can be edited. Sends to Signal, then overwrites the \
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
            sent_at_ms: input.target_timestamp.ms(),
        };
        check_target(ctx, to, target).await?;
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

pub struct DeleteMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteInput {
    recipient: String,
    target_timestamp: Timestamp,
}

#[async_trait]
impl Tool for DeleteMessage {
    fn name(&self) -> &'static str {
        "delete_message"
    }
    fn description(&self) -> &'static str {
        "Delete a message you sent to a contact for everyone in the chat (Signal 5.0+ \
         clients). recipient is the contact; target_timestamp names your message, as \
         get_conversation or a send result returns it, and it must be stored locally in that \
         conversation. Only your own messages can be deleted. \
         Irreversible. Sends to Signal, then flags the stored copy deleted."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "target_timestamp": Timestamp::schema("Your message to delete"),
            },
            "required": ["recipient", "target_timestamp"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: DeleteInput = parse(input)?;
        let to = recipient(ctx, &input.recipient).await?;
        delete_own(ctx, to, input.target_timestamp).await
    }
}

pub struct DeleteGroupMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteGroupInput {
    group_id: String,
    target_timestamp: Timestamp,
}

#[async_trait]
impl Tool for DeleteGroupMessage {
    fn name(&self) -> &'static str {
        "delete_group_message"
    }
    fn description(&self) -> &'static str {
        "Delete a message you sent to a group for all members (Signal 5.0+ clients). \
         group_id is the group, as list_conversations returns it; target_timestamp names your message, which must be stored \
         locally in that group. Only your own messages can be deleted. Irreversible. Sends to Signal, then flags the stored copy \
         deleted."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "group_id": group_schema(),
                "target_timestamp": Timestamp::schema("Your message to delete"),
            },
            "required": ["group_id", "target_timestamp"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: DeleteGroupInput = parse(input)?;
        let to = Recipient::Group(group(&input.group_id)?);
        delete_own(ctx, to, input.target_timestamp).await
    }
}

/// Deletes this account's own message in `to` for everyone.
async fn delete_own(
    ctx: &ToolContext,
    to: Recipient,
    sent_at: Timestamp,
) -> Result<Value, ToolError> {
    // NOTE: as in edit_message, the target is always this account's own message.
    let me = ctx.signal.account().await?.service_id;
    let target = MessageId {
        author: me,
        sent_at_ms: sent_at.ms(),
    };
    check_target(ctx, to, target).await?;
    ctx.signal.send(to, Outgoing::Delete { target }).await?;
    ctx.store
        .mark_deleted(target, me)
        .await
        .context("Signal accepted the delete, but the stored copy wasn't flagged")?;
    Ok(json!({ "status": "deleted" }))
}

/// Refuses `target` unless it's a live stored message in `conversation`.
// NOTE: recipient and target arrive as separate inputs. Without this check a
// call could name a message from one chat and send the change to another,
// leaking the edit's text there and rewriting the wrong stored message.
async fn check_target(
    ctx: &ToolContext,
    conversation: Recipient,
    target: MessageId,
) -> Result<(), ToolError> {
    let Some(message) = ctx.store.get(target).await? else {
        return Err(ToolError::InvalidInput(format!(
            "no stored message at {} by {}",
            target.sent_at_ms, target.author
        )));
    };
    if message.conversation != conversation {
        return Err(ToolError::InvalidInput(format!(
            "the message at {} isn't in that conversation",
            target.sent_at_ms
        )));
    }
    if message.deleted {
        return Err(ToolError::InvalidInput(format!(
            "the message at {} was deleted",
            target.sent_at_ms
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use relay_signal::{Account, FakeSignal, Group, Outgoing};
    use relay_store::MemoryStore;
    use serde_json::json;

    use crate::{Registry, ToolContext, ToolError, testing::*};
    use relay_core::prelude::*;

    /// A reaction must name the target by its author and timestamp and go to
    /// the conversation given, or it lands on the wrong message or nowhere.
    #[tokio::test]
    async fn react_to_message_sends_the_reaction_to_the_target_message() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store.insert(&incoming(2, 42, "hi")).await.unwrap();
        Registry::standard()
            .call(
                &ctx,
                "react_to_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "target_author": sid(2).to_string(),
                    "target_timestamp": 42,
                    "emoji": "👍",
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

    /// The delete goes to Signal as this account's own message, and only then
    /// is the stored copy flagged deleted.
    #[tokio::test]
    async fn delete_message_sends_the_delete_then_flags_the_stored_copy() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let dm = Recipient::Contact(sid(2));
        ctx.store.insert(&mine(dm, 42, "oops")).await.unwrap();
        Registry::standard()
            .call(
                &ctx,
                "delete_message",
                json!({"recipient": sid(2).to_string(), "target_timestamp": 42}),
            )
            .await
            .unwrap();
        let target = MessageId {
            author: sid(ME),
            sent_at_ms: 42,
        };
        assert_eq!(signal.sent(), [(dm, Outgoing::Delete { target })]);
        let stored = ctx.store.history(dm, None, 10).await.unwrap();
        assert!(stored[0].deleted, "delete not flagged: {stored:?}");
    }

    /// Someone else's message at the same timestamp must survive a delete, for
    /// the same reason as edit_message_cannot_rewrite_someone_elses_message.
    #[tokio::test]
    async fn delete_message_cannot_erase_someone_elses_message() {
        let (ctx, _) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store
            .insert(&incoming(2, 42, "their words"))
            .await
            .unwrap();
        let result = Registry::standard()
            .call(
                &ctx,
                "delete_message",
                json!({"recipient": sid(2).to_string(), "target_timestamp": 42}),
            )
            .await;
        let stored = ctx
            .store
            .history(Recipient::Contact(sid(2)), None, 10)
            .await
            .unwrap();
        assert!(!stored[0].deleted, "foreign message deleted: {result:?}");
    }

    /// A full-access context whose account is a member of `group`.
    fn in_group(group: GroupId) -> (ToolContext, Arc<FakeSignal>) {
        let signal = Arc::new(
            FakeSignal::new(Account {
                service_id: sid(ME),
                phone_number: None,
                kind: ProfileKind::Linked,
                device_name: "test".into(),
            })
            .with_groups(vec![Group {
                id: group,
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
        };
        (ctx, signal)
    }

    /// A group delete goes to the group as this account's own message, and only
    /// then is the stored copy flagged deleted.
    #[tokio::test]
    async fn delete_group_message_sends_the_delete_to_the_group_then_flags_it() {
        let group = GroupId::new([7; 32]);
        let (ctx, signal) = in_group(group);
        let conversation = Recipient::Group(group);
        ctx.store
            .insert(&mine(conversation, 42, "oops"))
            .await
            .unwrap();
        Registry::standard()
            .call(
                &ctx,
                "delete_group_message",
                json!({"group_id": group.to_string(), "target_timestamp": 42}),
            )
            .await
            .unwrap();
        let target = MessageId {
            author: sid(ME),
            sent_at_ms: 42,
        };
        assert_eq!(signal.sent(), [(conversation, Outgoing::Delete { target })]);
        let stored = ctx.store.history(conversation, None, 10).await.unwrap();
        assert!(stored[0].deleted, "delete not flagged: {stored:?}");
    }

    /// remove=true must go out as a retraction; sent as a plain reaction it
    /// would re-add the emoji it was meant to take back.
    #[tokio::test]
    async fn react_to_message_with_remove_sends_a_retraction() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store.insert(&incoming(2, 42, "hi")).await.unwrap();
        let out = Registry::standard()
            .call(
                &ctx,
                "react_to_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "target_author": sid(2).to_string(),
                    "target_timestamp": 42,
                    "emoji": "👍",
                    "remove": true,
                }),
            )
            .await
            .unwrap();
        let sent = signal.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [(_, Outgoing::Reaction { remove: true, .. })]
            ),
            "{sent:?}"
        );
        assert_eq!(out["status"], json!("reaction removed"), "{out}");
    }

    /// An edit naming my message from another conversation must be refused
    /// before sending, or the new text leaks into the wrong chat and the store
    /// rewrites a message that chat never saw edited.
    #[tokio::test]
    async fn an_edit_naming_a_message_from_another_conversation_is_refused_and_nothing_is_sent() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let elsewhere = Recipient::Contact(sid(3));
        ctx.store
            .insert(&mine(elsewhere, 42, "for 3 only"))
            .await
            .unwrap();
        let result = Registry::standard()
            .call(
                &ctx,
                "edit_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "target_timestamp": 42,
                    "message": "secret",
                }),
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "got {result:?}"
        );
        assert!(signal.sent().is_empty(), "sent {:?}", signal.sent());
        let stored = ctx.store.history(elsewhere, None, 10).await.unwrap();
        assert_eq!(stored[0].body.as_deref(), Some("for 3 only"), "{stored:?}");
    }

    /// A delete naming my message from another conversation must be refused
    /// before sending, or it erases a message the target chat can't see and
    /// flags the wrong stored copy deleted.
    #[tokio::test]
    async fn a_delete_naming_a_message_from_another_conversation_is_refused_and_nothing_is_sent() {
        let group = GroupId::new([7; 32]);
        let (ctx, signal) = in_group(group);
        let elsewhere = Recipient::Contact(sid(3));
        ctx.store
            .insert(&mine(elsewhere, 42, "for 3 only"))
            .await
            .unwrap();
        for (tool, field, conversation) in [
            ("delete_message", "recipient", sid(2).to_string()),
            ("delete_group_message", "group_id", group.to_string()),
        ] {
            let result = Registry::standard()
                .call(
                    &ctx,
                    tool,
                    json!({field: conversation, "target_timestamp": 42}),
                )
                .await;
            assert!(
                matches!(result, Err(ToolError::InvalidInput(_))),
                "{tool} gave {result:?}"
            );
        }
        assert!(signal.sent().is_empty(), "sent {:?}", signal.sent());
        let stored = ctx.store.history(elsewhere, None, 10).await.unwrap();
        assert!(!stored[0].deleted, "{stored:?}");
    }

    /// A reaction may land on anyone's message, but only in the conversation
    /// that holds it; aimed elsewhere it would reveal that message's existence
    /// to a chat that never saw it.
    #[tokio::test]
    async fn a_reaction_naming_a_message_from_another_conversation_is_refused_and_nothing_is_sent()
    {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store
            .insert(&incoming(3, 42, "to me only"))
            .await
            .unwrap();
        let result = Registry::standard()
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
            .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "got {result:?}"
        );
        assert!(signal.sent().is_empty(), "sent {:?}", signal.sent());
    }

    /// A deleted message is gone for everyone; editing it would send text
    /// that resurrects it on clients and un-delete nothing locally.
    #[tokio::test]
    async fn an_edit_of_a_deleted_message_is_refused_and_nothing_is_sent() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let dm = Recipient::Contact(sid(2));
        ctx.store.insert(&mine(dm, 42, "oops")).await.unwrap();
        let target = MessageId {
            author: sid(ME),
            sent_at_ms: 42,
        };
        ctx.store.mark_deleted(target, sid(ME)).await.unwrap();
        let result = Registry::standard()
            .call(
                &ctx,
                "edit_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "target_timestamp": 42,
                    "message": "back again",
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
