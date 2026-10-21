//! Tools that answer from the local store and the account, and never send.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{Limit, Offset, parse, recipient, recipient_schema},
};

pub struct ListConversations;

#[async_trait]
impl Tool for ListConversations {
    fn name(&self) -> &'static str {
        "list_conversations"
    }
    fn description(&self) -> &'static str {
        "List every conversation (direct and group) in the local store, most recent first. \
         Takes no parameters. Pass a row's recipient id to get_conversation to read it. \
         Local only: no Signal server call, nothing marked read."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, _input: Value) -> Result<Value, ToolError> {
        let conversations = ctx.store.conversations().await?;
        Ok(json!({ "conversations": conversations }))
    }
}

pub struct GetOwnNumber;

#[async_trait]
impl Tool for GetOwnNumber {
    fn name(&self) -> &'static str {
        "get_own_number"
    }
    fn description(&self) -> &'static str {
        "Return this account's own Signal number (E.164, null if Signal hasn't shared it with \
         this device) and service id. Messages whose sender equals service_id are your own. \
         Takes no parameters; no network call, no side effects."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, _input: Value) -> Result<Value, ToolError> {
        let account = ctx.signal.account().await?;
        Ok(json!({
            "number": account.phone_number,
            "service_id": account.service_id.to_string(),
        }))
    }
}

pub struct StoreStats;

#[async_trait]
impl Tool for StoreStats {
    fn name(&self) -> &'static str {
        "store_stats"
    }
    fn description(&self) -> &'static str {
        "Report what the local message store holds: message, conversation and unread counts, \
         and the oldest and newest message times in epoch milliseconds (null when empty). \
         Takes no parameters; local only."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, _input: Value) -> Result<Value, ToolError> {
        let stats = ctx.store.stats().await?;
        Ok(serde_json::to_value(stats).map_err(anyhow::Error::from)?)
    }
}

pub struct GetUnread;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetUnreadInput {
    #[serde(default)]
    limit: Limit,
}

#[async_trait]
impl Tool for GetUnread {
    fn name(&self) -> &'static str {
        "get_unread"
    }
    fn description(&self) -> &'static str {
        "Return unread incoming messages across all conversations, oldest first: the default \
         way to check for new messages. limit keeps the newest N; if has_more is true, call \
         again for the rest. Returned messages are marked read in the local store only; no \
         read receipt is sent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"limit": Limit::schema()},
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let GetUnreadInput { limit } = parse(input)?;
        let mut unread = ctx.store.unread().await?;
        unread.sort_by_key(|m| m.id.sent_at_ms);
        let has_more = unread.len() > limit.get() as usize;
        let messages = unread.split_off(unread.len().saturating_sub(limit.get() as usize));
        let ids: Vec<_> = messages.iter().map(|m| m.id).collect();
        ctx.store.mark_read(&ids).await?;
        Ok(json!({ "messages": messages, "has_more": has_more }))
    }
}

pub struct GetConversation;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetConversationInput {
    recipient: String,
    #[serde(default)]
    limit: Limit,
    #[serde(default)]
    offset: Offset,
}

#[async_trait]
impl Tool for GetConversation {
    fn name(&self) -> &'static str {
        "get_conversation"
    }
    fn description(&self) -> &'static str {
        "Read one conversation's history from the local store, oldest first. recipient comes \
         from list_conversations. limit is the page size; offset skips the newest N to page \
         back; has_more says whether older messages remain. Local only, nothing marked read."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "limit": Limit::schema(),
                "offset": Offset::schema(),
            },
            "required": ["recipient"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: GetConversationInput = parse(input)?;
        let conversation = recipient(&input.recipient)?;
        let (limit, offset) = (input.limit.get(), input.offset.get());
        let newest_first = ctx
            .store
            .history(conversation, None, offset + limit + 1)
            .await?;
        let has_more = newest_first.len() > (offset + limit) as usize;
        let mut messages: Vec<_> = newest_first
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        messages.reverse();
        Ok(json!({ "messages": messages, "has_more": has_more }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{Registry, testing::*};
    use relay_core::prelude::*;

    /// The inbox must come back newest conversation first, so "who wrote last"
    /// is the first row a client sees.
    #[tokio::test]
    async fn list_conversations_puts_the_most_recent_first() {
        let (ctx, _) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store.upsert(&incoming(2, 100, "old")).await.unwrap();
        ctx.store.upsert(&incoming(3, 200, "new")).await.unwrap();
        let out = Registry::standard()
            .call(&ctx, "list_conversations", json!({}))
            .await
            .unwrap();
        let ids: Vec<_> = out["conversations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["recipient"]["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids, [sid(3).to_string(), sid(2).to_string()], "{out}");
    }

    /// Clients tell their own messages apart by the account's number and id,
    /// so both must come from the live account.
    #[tokio::test]
    async fn get_own_number_reports_the_accounts_number_and_service_id() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        let out = Registry::standard()
            .call(&ctx, "get_own_number", json!({}))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({"number": "+15550000001", "service_id": sid(ME).to_string()})
        );
    }

    /// Stats must reflect what's stored, including the date bounds clients use
    /// to tell whether history was imported.
    #[tokio::test]
    async fn store_stats_counts_messages_and_reports_date_bounds() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        ctx.store.upsert(&incoming(2, 100, "a")).await.unwrap();
        ctx.store.upsert(&incoming(3, 300, "b")).await.unwrap();
        let out = Registry::standard()
            .call(&ctx, "store_stats", json!({}))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({"messages": 2, "conversations": 2, "unread": 2, "oldest_ms": 100, "newest_ms": 300})
        );
    }

    fn bodies(messages: &Value) -> Vec<String> {
        messages
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].as_str().unwrap().to_owned())
            .collect()
    }

    /// get_unread is how a client checks for new mail, so a message must come
    /// back once and only once, oldest first; with a limit the newest are kept
    /// and the rest stay unread for the next call.
    #[tokio::test]
    async fn get_unread_returns_each_message_once_keeping_the_newest_under_a_limit() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        for (t, body) in [(100, "a"), (200, "b"), (300, "c")] {
            ctx.store.upsert(&incoming(2, t, body)).await.unwrap();
        }
        let registry = Registry::standard();
        let first = registry
            .call(&ctx, "get_unread", json!({"limit": 2}))
            .await
            .unwrap();
        assert_eq!(bodies(&first["messages"]), ["b", "c"], "{first}");
        assert_eq!(first["has_more"], json!(true), "{first}");
        let second = registry.call(&ctx, "get_unread", json!({})).await.unwrap();
        assert_eq!(bodies(&second["messages"]), ["a"], "{second}");
        assert_eq!(second["has_more"], json!(false), "{second}");
    }

    /// Paging back through a chat: offset skips the newest N, the page reads
    /// oldest first, and other conversations never leak in.
    #[tokio::test]
    async fn get_conversation_pages_back_from_the_newest_and_reads_oldest_first() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        for (t, body) in [(100, "a"), (200, "b"), (300, "c"), (400, "d")] {
            ctx.store.upsert(&incoming(2, t, body)).await.unwrap();
        }
        ctx.store
            .upsert(&incoming(3, 250, "other chat"))
            .await
            .unwrap();
        let out = Registry::standard()
            .call(
                &ctx,
                "get_conversation",
                json!({"recipient": sid(2).to_string(), "limit": 2, "offset": 1}),
            )
            .await
            .unwrap();
        assert_eq!(bodies(&out["messages"]), ["b", "c"], "{out}");
        assert_eq!(out["has_more"], json!(true), "{out}");
    }
}
