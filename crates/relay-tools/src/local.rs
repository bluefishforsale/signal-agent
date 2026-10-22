//! Tools that manage the local message store itself: export, read state, and
//! deleting history. None of them talk to Signal.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use relay_core::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{Instant, MessageRef, parse, recipient, recipient_schema},
};

pub struct ExportMessages;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Format {
    #[default]
    Json,
    Csv,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportMessagesInput {
    #[serde(default)]
    format: Format,
    recipient: Option<String>,
    since: Option<Instant>,
}

#[async_trait]
impl Tool for ExportMessages {
    fn name(&self) -> &'static str {
        "export_messages"
    }
    fn description(&self) -> &'static str {
        "Export messages from the local store as one string, oldest first, for archiving or \
         analysis. format is json (default; full message objects, as get_conversation returns \
         them) or csv (one row per message, attachments as a count). recipient keeps one conversation; since keeps messages at or after a time. \
         Returns {format, data}. Local only: only messages already in the store are included, \
         nothing is marked read."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "format": {
                    "type": "string",
                    "enum": ["json", "csv"],
                    "default": "json",
                    "description": "Output format (default json)",
                },
                "recipient": recipient_schema(),
                "since": Instant::schema("Only messages at or after this time"),
            },
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: ExportMessagesInput = parse(input)?;
        let conversation = match input.recipient {
            Some(r) => Some(recipient(ctx, &r).await?),
            None => None,
        };
        let messages = ctx
            .store
            .export(conversation, input.since.map(Instant::ms))
            .await?;
        Ok(match input.format {
            Format::Json => {
                let data = serde_json::to_string(&messages).map_err(anyhow::Error::from)?;
                json!({ "format": "json", "data": data })
            }
            Format::Csv => json!({ "format": "csv", "data": csv(&messages) }),
        })
    }
}

pub struct MarkAsUnread;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkAsUnreadInput {
    message_ids: Vec<MessageRef>,
}

#[async_trait]
impl Tool for MarkAsUnread {
    fn name(&self) -> &'static str {
        "mark_as_unread"
    }
    fn description(&self) -> &'static str {
        "Mark messages unread again in the local store, so get_unread returns them on its next \
         call, e.g. to flag them for follow-up. message_ids are the id objects get_conversation, \
         get_unread or search_messages return; unknown ids are ignored. Idempotent. Returns \
         {status, count}, count being how many ids were passed. Local only: read receipts \
         already sent and other devices are unaffected."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message_ids": {
                    "type": "array",
                    "items": MessageRef::schema(),
                    "minItems": 1,
                    "maxItems": MAX_IDS,
                    "description": "The messages to mark unread",
                },
            },
            "required": ["message_ids"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let MarkAsUnreadInput { message_ids } = parse(input)?;
        if !(1..=MAX_IDS).contains(&message_ids.len()) {
            return Err(ToolError::InvalidInput(format!(
                "message_ids must hold 1 to {MAX_IDS} ids, got {}",
                message_ids.len()
            )));
        }
        let ids: Vec<_> = message_ids.into_iter().map(MessageRef::id).collect();
        ctx.store.mark_unread(&ids).await?;
        Ok(json!({ "status": "marked unread", "count": ids.len() }))
    }
}

pub struct ClearLocalStore;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClearLocalStoreInput {
    confirm: bool,
}

#[async_trait]
impl Tool for ClearLocalStore {
    fn name(&self) -> &'static str {
        "clear_local_store"
    }
    fn description(&self) -> &'static str {
        "Delete ALL messages from the local store. confirm must be exactly true, otherwise \
         nothing is deleted and an error is returned. Local only: nothing is deleted from \
         Signal, your phone or other devices. Irreversible except by importing or receiving \
         again. Returns {deleted, status}. Use prune_store to drop only old messages."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "confirm": {
                    "type": "boolean",
                    "description": "Must be true to proceed; guards against accidental deletion",
                },
            },
            "required": ["confirm"],
            "additionalProperties": false,
        })
    }
    // NOTE: true although nothing reaches Signal. This destroys history the
    // account can't send back, so a read-only profile must not be able to.
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let ClearLocalStoreInput { confirm } = parse(input)?;
        if !confirm {
            return Err(ToolError::InvalidInput(
                "clear_local_store deletes every stored message; pass confirm: true to proceed"
                    .into(),
            ));
        }
        let deleted = ctx.store.prune(EVERYTHING).await?;
        Ok(json!({ "deleted": deleted, "status": "cleared" }))
    }
}

pub struct DeleteLocalMessages;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteLocalMessagesInput {
    recipient: String,
    confirm: bool,
}

#[async_trait]
impl Tool for DeleteLocalMessages {
    fn name(&self) -> &'static str {
        "delete_local_messages"
    }
    fn description(&self) -> &'static str {
        "Delete one conversation's messages from the local store, with its read state and \
         title. recipient is the conversation (as list_conversations names it, group ids \
         included); confirm must be exactly true, otherwise nothing is deleted and an error is \
         returned. Local only: nothing is deleted from Signal, your phone or other devices. \
         Irreversible. Returns {deleted, status}. Use clear_local_store to delete everything."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "confirm": {
                    "type": "boolean",
                    "description": "Must be true to proceed; guards against accidental deletion",
                },
            },
            "required": ["recipient", "confirm"],
            "additionalProperties": false,
        })
    }
    // NOTE: true for the same reason as clear_local_store: it destroys history
    // a read-only profile must not be able to.
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: DeleteLocalMessagesInput = parse(input)?;
        if !input.confirm {
            return Err(ToolError::InvalidInput(
                "delete_local_messages deletes the conversation's stored messages; pass \
                 confirm: true to proceed"
                    .into(),
            ));
        }
        let conversation = recipient(ctx, &input.recipient).await?;
        let deleted = ctx.store.delete_conversation(conversation).await?;
        Ok(json!({ "deleted": deleted, "status": "deleted" }))
    }
}

pub struct PruneStore;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PruneStoreInput {
    #[serde(default)]
    days: Days,
}

/// How old a message must be to prune: 1 to 36500 days, 180 when not given.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "u64")]
struct Days(u64);

impl Days {
    const MAX: u64 = 36_500;
}

impl Default for Days {
    fn default() -> Self {
        Self(180)
    }
}

impl TryFrom<u64> for Days {
    type Error = String;
    fn try_from(n: u64) -> Result<Self, String> {
        if !(1..=Self::MAX).contains(&n) {
            return Err(format!("days must be 1 to {}, got {n}", Self::MAX));
        }
        Ok(Self(n))
    }
}

#[async_trait]
impl Tool for PruneStore {
    fn name(&self) -> &'static str {
        "prune_store"
    }
    fn description(&self) -> &'static str {
        "Delete messages older than a number of days from the local store. days (1 to 36500, \
         default 180): messages sent before now minus this many days are deleted. Local only: \
         nothing is deleted from Signal. Irreversible locally. Returns {deleted, \
         older_than_days}. Use clear_local_store to wipe everything."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": Days::MAX,
                    "default": 180,
                    "description": "Delete messages older than this many days (default 180)",
                },
            },
            "additionalProperties": false,
        })
    }
    // NOTE: true for the same reason as clear_local_store: it destroys history.
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let PruneStoreInput { days } = parse(input)?;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(anyhow::Error::from)?
            .as_millis() as u64;
        let deleted = ctx
            .store
            .prune(now_ms.saturating_sub(days.0 * 86_400_000))
            .await?;
        Ok(json!({ "deleted": deleted, "older_than_days": days.0 }))
    }
}

/// A prune cutoff later than any stored message.
// NOTE: not u64::MAX. The SQLCipher store binds the cutoff as an i64 and
// refuses anything larger, so u64::MAX would fail the clear outright.
const EVERYTHING: u64 = i64::MAX as u64;

/// The most ids one call takes, matching the largest page a read tool returns.
const MAX_IDS: usize = 500;

fn csv(messages: &[Message]) -> String {
    let mut out = String::from(
        "author,sent_at_ms,conversation_kind,conversation_id,sender,from_me,received_at_ms,\
         body,quote_author,quote_sent_at_ms,attachments,edited,deleted\r\n",
    );
    for m in messages {
        let (kind, conversation) = match m.conversation {
            Recipient::Contact(id) => ("contact", id.to_string()),
            Recipient::Group(id) => ("group", id.to_string()),
        };
        let row = [
            m.id.author.to_string(),
            m.id.sent_at_ms.to_string(),
            kind.to_owned(),
            conversation,
            m.sender.to_string(),
            m.from_me.to_string(),
            m.received_at_ms.to_string(),
            csv_field(m.body.as_deref().unwrap_or_default()),
            m.quote.map(|q| q.author.to_string()).unwrap_or_default(),
            m.quote
                .map(|q| q.sent_at_ms.to_string())
                .unwrap_or_default(),
            m.attachments.len().to_string(),
            m.edited.to_string(),
            m.deleted.to_string(),
        ];
        out.push_str(&row.join(","));
        out.push_str("\r\n");
    }
    out
}

/// A free-text field, made safe for a spreadsheet then quoted per RFC 4180:
/// a field holding a separator, quote or line break is quoted, with inner
/// quotes doubled.
// NOTE: bodies come from other people. Excel and Sheets run a cell starting
// with = + - @ tab or CR as a formula, so without the leading quote a stranger's
// message could run on whoever opens the export.
fn csv_field(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{s}")
    } else {
        s.to_owned()
    };
    if !s.contains([',', '"', '\n', '\r']) {
        return s;
    }
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{Registry, ToolError, testing::*};
    use relay_core::prelude::*;

    /// An export must hold exactly what the filters ask for, oldest first, in
    /// the same message shape the read tools return.
    #[tokio::test]
    async fn export_messages_json_keeps_one_conversation_since_an_instant() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        for (who, t, body) in [
            (2, 999, "too old"),
            (2, 3000, "b"),
            (2, 1000, "a"),
            (3, 2000, "other"),
        ] {
            ctx.store.insert(&incoming(who, t, body)).await.unwrap();
        }
        let out = Registry::standard()
            .call(
                &ctx,
                "export_messages",
                json!({"recipient": sid(2).to_string(), "since": "1970-01-01T00:00:01Z"}),
            )
            .await
            .unwrap();
        assert_eq!(out["format"], json!("json"), "{out}");
        let data: Value = serde_json::from_str(out["data"].as_str().unwrap()).unwrap();
        let expected = ctx
            .store
            .export(Some(Recipient::Contact(sid(2))), Some(1000))
            .await
            .unwrap();
        assert_eq!(data, json!(expected), "{out}");
    }

    /// CSV goes into spreadsheets, so a body with commas, quotes or line
    /// breaks must stay one field rather than spilling into the next columns.
    #[tokio::test]
    async fn export_messages_csv_quotes_bodies_with_commas_quotes_and_newlines() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        ctx.store
            .insert(&incoming(2, 1000, "say \"hi\", then\nbye"))
            .await
            .unwrap();
        let out = Registry::standard()
            .call(&ctx, "export_messages", json!({"format": "csv"}))
            .await
            .unwrap();
        assert_eq!(out["format"], json!("csv"), "{out}");
        let s = sid(2);
        let expected = format!(
            "author,sent_at_ms,conversation_kind,conversation_id,sender,from_me,received_at_ms,\
             body,quote_author,quote_sent_at_ms,attachments,edited,deleted\r\n\
             {s},1000,contact,{s},{s},false,1000,\"say \"\"hi\"\", then\nbye\",,,0,false,false\r\n"
        );
        assert_eq!(out["data"].as_str().unwrap(), expected, "{out}");
    }

    /// A client flags a message for follow-up by passing back the id exactly
    /// as get_unread returned it; only that message comes back as unread.
    #[tokio::test]
    async fn mark_as_unread_takes_ids_as_read_tools_return_them() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        ctx.store.insert(&incoming(2, 1000, "a")).await.unwrap();
        ctx.store.insert(&incoming(2, 2000, "b")).await.unwrap();
        let registry = Registry::standard();
        let read = registry.call(&ctx, "get_unread", json!({})).await.unwrap();
        let first_id = read["messages"][0]["id"].clone();
        let out = registry
            .call(&ctx, "mark_as_unread", json!({"message_ids": [first_id]}))
            .await
            .unwrap();
        assert_eq!(out["count"], json!(1), "{out}");
        let again = registry.call(&ctx, "get_unread", json!({})).await.unwrap();
        let bodies: Vec<_> = again["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].clone())
            .collect();
        assert_eq!(bodies, [json!("a")], "{again}");
    }

    /// Wiping history must take an explicit yes: confirm false is refused
    /// with a reason and leaves every message where it was.
    #[tokio::test]
    async fn clear_local_store_without_confirm_leaves_the_store_untouched() {
        let (ctx, _) = ctx(AccessMode::Full, ProfileKind::Primary);
        ctx.store
            .insert(&incoming(2, 1000, "keep me"))
            .await
            .unwrap();
        let result = Registry::standard()
            .call(&ctx, "clear_local_store", json!({"confirm": false}))
            .await;
        let Err(ToolError::InvalidInput(why)) = result else {
            panic!("unconfirmed clear gave {result:?}");
        };
        assert!(
            why.contains("confirm"),
            "reason doesn't mention confirm: {why}"
        );
        assert_eq!(
            ctx.store.stats().await.unwrap().messages,
            1,
            "store changed"
        );
    }

    /// A confirmed clear empties the store, however new the newest message,
    /// and reports how many it removed.
    #[tokio::test]
    async fn clear_local_store_with_confirm_removes_every_message() {
        let (ctx, _) = ctx(AccessMode::Full, ProfileKind::Primary);
        ctx.store.insert(&incoming(2, 1000, "old")).await.unwrap();
        ctx.store
            .insert(&incoming(3, 4_102_444_799_000, "2099"))
            .await
            .unwrap();
        let out = Registry::standard()
            .call(&ctx, "clear_local_store", json!({"confirm": true}))
            .await
            .unwrap();
        assert_eq!(out, json!({"deleted": 2, "status": "cleared"}));
        assert_eq!(ctx.store.stats().await.unwrap().messages, 0, "{out}");
    }

    /// Local deletes never reach Signal but still destroy history, so a
    /// read-only profile must be refused every one of them, even confirmed,
    /// with the store left as it was.
    #[tokio::test]
    async fn read_only_refuses_every_destructive_local_tool() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Primary);
        ctx.store
            .insert(&incoming(2, 1000, "keep me"))
            .await
            .unwrap();
        let registry = Registry::standard();
        for (name, input) in [
            ("clear_local_store", json!({"confirm": true})),
            ("prune_store", json!({"days": 1})),
        ] {
            let result = registry.call(&ctx, name, input).await;
            assert!(
                matches!(result, Err(ToolError::ReadOnly(_))),
                "{name} in read-only mode gave {result:?}"
            );
        }
        assert_eq!(
            ctx.store.stats().await.unwrap().messages,
            1,
            "store changed"
        );
    }

    /// days counts back from now: a message from yesterday survives a 2-day
    /// prune and one from three days ago doesn't.
    #[tokio::test]
    async fn prune_store_drops_only_messages_older_than_days_before_now() {
        let (ctx, _) = ctx(AccessMode::Full, ProfileKind::Primary);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let day_ms = 86_400_000;
        ctx.store
            .insert(&incoming(2, now_ms - 3 * day_ms, "three days ago"))
            .await
            .unwrap();
        ctx.store
            .insert(&incoming(2, now_ms - day_ms, "yesterday"))
            .await
            .unwrap();
        let out = Registry::standard()
            .call(&ctx, "prune_store", json!({"days": 2}))
            .await
            .unwrap();
        assert_eq!(out, json!({"deleted": 1, "older_than_days": 2}));
        let left = ctx.store.export(None, None).await.unwrap();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(left[0].body.as_deref(), Some("yesterday"), "{left:?}");
    }

    /// Bodies come from other people, and a spreadsheet runs a cell starting
    /// with a formula character, so an exported message starting with one
    /// can't run as a formula: it gets a leading quote. An ordinary
    /// "- item" body takes the same prefix and stays readable.
    #[tokio::test]
    async fn an_exported_message_starting_with_a_formula_character_cant_run_as_a_formula() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        let bodies = [
            "=HYPERLINK(\"http://evil\",\"x\")",
            "+1+1",
            "- buy milk",
            "@SUM(A1)",
            "\t=1",
            "\r=1",
        ];
        for (t, body) in bodies.iter().enumerate() {
            ctx.store
                .insert(&incoming(2, 1000 + t as u64, body))
                .await
                .unwrap();
        }
        let out = Registry::standard()
            .call(&ctx, "export_messages", json!({"format": "csv"}))
            .await
            .unwrap();
        let data = out["data"].as_str().unwrap();
        for (body, cell) in [
            (
                "=HYPERLINK",
                "\"'=HYPERLINK(\"\"http://evil\"\",\"\"x\"\")\"",
            ),
            ("+1+1", ",'+1+1,"),
            ("- buy milk", ",'- buy milk,"),
            ("@SUM", ",'@SUM(A1),"),
            ("tab", ",'\t=1,"),
            ("CR", ",\"'\r=1\","),
        ] {
            assert!(
                data.contains(cell),
                "{body} not neutralized as {cell:?}: {data:?}"
            );
        }
    }

    /// Deleting one conversation must leave every other one alone, and only
    /// with confirm: true, or a slip wipes history the account can't get back.
    #[tokio::test]
    async fn delete_local_messages_removes_one_conversation_only_when_confirmed() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Primary);
        for (sender, at) in [(2, 1), (2, 2), (3, 3)] {
            ctx.store.insert(&incoming(sender, at, "hi")).await.unwrap();
        }
        let registry = Registry::standard();
        let unconfirmed = registry
            .call(
                &ctx,
                "delete_local_messages",
                json!({"recipient": sid(2).to_string(), "confirm": false}),
            )
            .await;
        assert!(
            matches!(unconfirmed, Err(ToolError::InvalidInput(_))),
            "unconfirmed delete gave {unconfirmed:?}"
        );
        assert_eq!(ctx.store.stats().await.unwrap().messages, 3);
        let out = registry
            .call(
                &ctx,
                "delete_local_messages",
                json!({"recipient": sid(2).to_string(), "confirm": true}),
            )
            .await
            .unwrap();
        assert_eq!(out["deleted"], json!(2), "{out}");
        let left = ctx.store.export(None, None).await.unwrap();
        assert!(
            left.len() == 1 && left[0].conversation == Recipient::Contact(sid(3)),
            "{left:?}"
        );
        assert!(signal.sent().is_empty(), "a local delete reached Signal");
    }
}
