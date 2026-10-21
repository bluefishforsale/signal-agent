//! Tools that answer from the local store and the account, and never send.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolError};

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

#[cfg(test)]
mod tests {
    use serde_json::json;

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
}
