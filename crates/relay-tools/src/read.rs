//! Tools that answer from the local store and the account, and never send.

use async_trait::async_trait;
use relay_store::SearchQuery;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{
        Instant, Limit, Offset, contact, contact_schema, non_blank, parse, recipient,
        recipient_schema,
    },
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

pub struct ListAccounts;

#[async_trait]
impl Tool for ListAccounts {
    fn name(&self) -> &'static str {
        "list_accounts"
    }
    fn description(&self) -> &'static str {
        "List the Signal account this server speaks for: number (E.164, null if Signal hasn't \
         shared it with this device), service_id, kind (linked or primary) and device_name. \
         Each account on this machine is its own MCP server, so there is always exactly one. \
         Takes no parameters; no network call."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, _input: Value) -> Result<Value, ToolError> {
        let account = ctx.signal.account().await?;
        Ok(json!({"accounts": [{
            "number": account.phone_number,
            "service_id": account.service_id.to_string(),
            "kind": account.kind,
            "device_name": account.device_name,
        }]}))
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
        unread_schema()
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        take_unread(ctx, input).await
    }
}

fn unread_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"limit": Limit::schema()},
        "additionalProperties": false,
    })
}

async fn take_unread(ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
    let GetUnreadInput { limit } = parse(input)?;
    let mut unread = ctx.store.unread().await?;
    unread.sort_by_key(|m| m.id.sent_at_ms);
    let has_more = unread.len() > limit.get() as usize;
    let messages = unread.split_off(unread.len().saturating_sub(limit.get() as usize));
    let ids: Vec<_> = messages.iter().map(|m| m.id).collect();
    ctx.store.mark_read(&ids).await?;
    Ok(json!({ "messages": messages, "has_more": has_more }))
}

// NOTE: the agent receives continuously, so there is nothing left for a tool
// to poll. These two keep upstream's names and hand out get_unread's queue;
// giving them a second cursor would show a client every message twice.
pub struct ReceiveMessages;

#[async_trait]
impl Tool for ReceiveMessages {
    fn name(&self) -> &'static str {
        "receive_messages"
    }
    fn description(&self) -> &'static str {
        "Return messages that arrived since any client last checked, oldest first. It \
         consumes the unread queue it shares with get_unread and receive_direct: each message \
         comes back from exactly one of them, once. The background agent receives from \
         Signal all the time, so this returns at once. limit keeps the newest N; if has_more is true, call again. \
         Marks them read in the local store only; no read receipt is sent."
    }
    fn input_schema(&self) -> Value {
        unread_schema()
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        take_unread(ctx, input).await
    }
}

pub struct ReceiveDirect;

#[async_trait]
impl Tool for ReceiveDirect {
    fn name(&self) -> &'static str {
        "receive_direct"
    }
    fn description(&self) -> &'static str {
        "The same as receive_messages, kept for clients that call it by this name: there is \
         no separate daemon to bypass, the background agent already holds the connection. \
         Returns messages that arrived since any client last checked, oldest first, and \
         consumes the unread queue get_unread and receive_messages share: each message comes \
         back from exactly one of them, once. Marks them read in the local store \
         only; no read receipt is sent."
    }
    fn input_schema(&self) -> Value {
        unread_schema()
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        take_unread(ctx, input).await
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
    since: Option<Instant>,
}

#[async_trait]
impl Tool for GetConversation {
    fn name(&self) -> &'static str {
        "get_conversation"
    }
    fn description(&self) -> &'static str {
        "Read one conversation's history from the local store, oldest first. recipient comes \
         from list_conversations. limit is the page size; offset skips the newest N to page \
         back; since keeps only messages at or after a time; has_more says whether older \
         messages remain. Local only, nothing marked read."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "limit": Limit::schema(),
                "offset": Offset::schema(),
                "since": Instant::schema("Only messages at or after this time"),
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
        let conversation = recipient(ctx, &input.recipient).await?;
        let (limit, offset) = (input.limit.get(), input.offset.get());
        let since_ms = input.since.map_or(0, Instant::ms);
        let newest_first: Vec<_> = ctx
            .store
            .history(conversation, None, offset + limit + 1)
            .await?
            .into_iter()
            .take_while(|m| m.id.sent_at_ms >= since_ms)
            .collect();
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

pub struct SearchMessages;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchMessagesInput {
    query: String,
    sender: Option<String>,
    since: Option<Instant>,
    until: Option<Instant>,
    #[serde(default)]
    limit: Limit,
    #[serde(default)]
    offset: Offset,
}

#[async_trait]
impl Tool for SearchMessages {
    fn name(&self) -> &'static str {
        "search_messages"
    }
    fn description(&self) -> &'static str {
        "Full-text search of message bodies across every conversation in the local store, \
         newest first. query is the words to find. sender keeps one person's messages; since \
         (inclusive) and until (exclusive) bound the time; limit and offset page through \
         results. Local only, nothing marked read."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "description": "Words to find"},
                "sender": contact_schema("Only messages from this person"),
                "since": Instant::schema("Only messages at or after this time"),
                "until": Instant::schema("Only messages before this time"),
                "limit": Limit::schema(),
                "offset": Offset::schema(),
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SearchMessagesInput = parse(input)?;
        let sender = match input.sender {
            Some(s) => Some(contact(ctx, &s).await?),
            None => None,
        };
        let query = SearchQuery {
            text: non_blank("query", input.query)?,
            sender,
            conversation: None,
            since_ms: input.since.map(Instant::ms),
            until_ms: input.until.map(Instant::ms),
            limit: input.limit.get(),
            offset: input.offset.get(),
        };
        let messages = ctx.store.search(&query).await?;
        Ok(json!({ "messages": messages }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use relay_signal::Contact;

    use crate::{Registry, ToolError, testing::*};
    use relay_core::prelude::*;

    /// The inbox must come back newest conversation first, so "who wrote last"
    /// is the first row a client sees.
    #[tokio::test]
    async fn list_conversations_puts_the_most_recent_first() {
        let (ctx, _) = ctx(AccessMode::Full, ProfileKind::Linked);
        ctx.store.insert(&incoming(2, 100, "old")).await.unwrap();
        ctx.store.insert(&incoming(3, 200, "new")).await.unwrap();
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

    /// Each profile is its own MCP server, and a client scoped to one profile
    /// must not learn the other accounts on the machine, so list_accounts
    /// reports exactly this profile's account.
    #[tokio::test]
    async fn list_accounts_reports_only_this_profiles_account() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        let out = Registry::standard()
            .call(&ctx, "list_accounts", json!({}))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({"accounts": [{
                "number": "+15550000001",
                "service_id": sid(ME).to_string(),
                "kind": "linked",
                "device_name": "test",
            }]})
        );
    }

    /// Stats must reflect what's stored, including the date bounds clients use
    /// to tell whether history was imported.
    #[tokio::test]
    async fn store_stats_counts_messages_and_reports_date_bounds() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        ctx.store.insert(&incoming(2, 100, "a")).await.unwrap();
        ctx.store.insert(&incoming(3, 300, "b")).await.unwrap();
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
            ctx.store.insert(&incoming(2, t, body)).await.unwrap();
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

    /// The agent receives continuously, so receive_messages and receive_direct
    /// are views of the same new-mail queue as get_unread: a message any of
    /// them returned must never come back from another.
    #[tokio::test]
    async fn receive_tools_share_get_unreads_queue_and_return_each_message_once() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        let registry = Registry::standard();
        let mut seen = vec![];
        for (t, tool) in [
            (100, "receive_messages"),
            (200, "receive_direct"),
            (300, "get_unread"),
        ] {
            ctx.store.insert(&incoming(2, t, tool)).await.unwrap();
            let out = registry.call(&ctx, tool, json!({})).await.unwrap();
            seen.extend(bodies(&out["messages"]));
        }
        assert_eq!(seen, ["receive_messages", "receive_direct", "get_unread"]);
    }

    /// Paging back through a chat: offset skips the newest N, the page reads
    /// oldest first, and other conversations never leak in.
    #[tokio::test]
    async fn get_conversation_pages_back_from_the_newest_and_reads_oldest_first() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        for (t, body) in [(100, "a"), (200, "b"), (300, "c"), (400, "d")] {
            ctx.store.insert(&incoming(2, t, body)).await.unwrap();
        }
        ctx.store
            .insert(&incoming(3, 250, "other chat"))
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

    /// Upstream tools address people by phone number, so a number must reach
    /// the same conversation as the contact's service id; a number that isn't a
    /// contact is a caller mistake, not an empty chat.
    #[tokio::test]
    async fn get_conversation_resolves_a_contacts_phone_number_and_refuses_strangers() {
        let contact = Contact {
            service_id: sid(2),
            name: Some("Ada".into()),
            phone_number: Some("+15550000002".into()),
        };
        let (ctx, _) = ctx_with(AccessMode::ReadOnly, ProfileKind::Linked, vec![contact]);
        ctx.store.insert(&incoming(2, 100, "hi")).await.unwrap();
        let registry = Registry::standard();
        let out = registry
            .call(
                &ctx,
                "get_conversation",
                json!({"recipient": "+15550000002"}),
            )
            .await
            .unwrap();
        assert_eq!(bodies(&out["messages"]), ["hi"], "{out}");
        let stranger = registry
            .call(
                &ctx,
                "get_conversation",
                json!({"recipient": "+15550000009"}),
            )
            .await;
        assert!(
            matches!(stranger, Err(ToolError::InvalidInput(_))),
            "unknown number gave {stranger:?}"
        );
    }

    const JAN_1_2024_MS: u64 = 1_704_067_200_000;
    const DAY_MS: u64 = 86_400_000;

    /// since is inclusive, read as UTC, and accepts a bare date or a full
    /// datetime; older messages must not count toward has_more.
    #[tokio::test]
    async fn get_conversation_since_keeps_messages_at_or_after_the_instant() {
        let (ctx, _) = ctx(AccessMode::ReadOnly, ProfileKind::Linked);
        for (t, body) in [
            (JAN_1_2024_MS - 1, "dec 31"),
            (JAN_1_2024_MS, "midnight"),
            (JAN_1_2024_MS + DAY_MS + 3_600_000, "jan 2 1am"),
        ] {
            ctx.store.insert(&incoming(2, t, body)).await.unwrap();
        }
        let registry = Registry::standard();
        let call = |since: &'static str| {
            registry.call(
                &ctx,
                "get_conversation",
                json!({"recipient": sid(2).to_string(), "since": since}),
            )
        };
        let out = call("2024-01-01").await.unwrap();
        assert_eq!(bodies(&out["messages"]), ["midnight", "jan 2 1am"], "{out}");
        assert_eq!(out["has_more"], json!(false), "{out}");
        let out = call("2024-01-02T01:00:00Z").await.unwrap();
        assert_eq!(bodies(&out["messages"]), ["jan 2 1am"], "{out}");
    }

    /// Every filter must reach the store: text, sender (by number or id),
    /// since inclusive, until exclusive, and offset paging over newest first.
    #[tokio::test]
    async fn search_messages_applies_text_sender_window_and_paging() {
        let contact = Contact {
            service_id: sid(2),
            name: None,
            phone_number: Some("+15550000002".into()),
        };
        let (ctx, _) = ctx_with(AccessMode::ReadOnly, ProfileKind::Linked, vec![contact]);
        for (who, t, body) in [
            (2, JAN_1_2024_MS - 1, "lunch dec 31"),
            (2, JAN_1_2024_MS, "lunch jan 1"),
            (2, JAN_1_2024_MS + 1000, "lunch jan 1 again"),
            (3, JAN_1_2024_MS + 2000, "lunch from someone else"),
            (2, JAN_1_2024_MS + 3000, "dinner jan 1"),
            (2, JAN_1_2024_MS + DAY_MS, "lunch jan 2"),
        ] {
            ctx.store.insert(&incoming(who, t, body)).await.unwrap();
        }
        let out = Registry::standard()
            .call(
                &ctx,
                "search_messages",
                json!({
                    "query": "LUNCH",
                    "sender": "+15550000002",
                    "since": "2024-01-01",
                    "until": "2024-01-02",
                    "limit": 1,
                    "offset": 1,
                }),
            )
            .await
            .unwrap();
        assert_eq!(bodies(&out["messages"]), ["lunch jan 1"], "{out}");
    }
}
