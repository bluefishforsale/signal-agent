//! Tools that manage the local message store itself: export, read state, and
//! deleting history. None of them talk to Signal.

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

/// RFC 4180: a field holding a separator, quote or line break is quoted, with
/// inner quotes doubled.
fn csv_field(s: &str) -> String {
    if !s.contains([',', '"', '\n', '\r']) {
        return s.to_owned();
    }
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{Registry, testing::*};
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
}
