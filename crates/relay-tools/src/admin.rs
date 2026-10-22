//! Deleting another member's group message on this account's authority as a
//! group admin. The store can't tell who is deleting, so the admin check lives
//! here, against Signal's own member roles.

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::Outgoing;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{Timestamp, contact, contact_schema, group, group_schema, parse},
    mutate::check_target,
};

pub struct AdminDeleteMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdminDeleteInput {
    group_id: String,
    target_author: String,
    target_timestamp: Timestamp,
}

#[async_trait]
impl Tool for AdminDeleteMessage {
    fn name(&self) -> &'static str {
        "admin_delete_message"
    }
    fn description(&self) -> &'static str {
        "As a group admin, delete another member's message for everyone in the group. \
         group_id is the group, as list_conversations returns it; target_author and \
         target_timestamp name the message, as get_conversation returns it, and it must be \
         stored locally in that group. Refused unless Signal lists you as an admin of the \
         group. Use delete_group_message for your own messages. Irreversible. Sends to \
         Signal, then flags the stored copy deleted."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "group_id": group_schema(),
                "target_author": contact_schema("Who wrote the message"),
                "target_timestamp": Timestamp::schema("The message to delete"),
            },
            "required": ["group_id", "target_author", "target_timestamp"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: AdminDeleteInput = parse(input)?;
        let group_id = group(&input.group_id)?;
        let target = MessageId {
            author: contact(ctx, &input.target_author).await?,
            sent_at_ms: input.target_timestamp.ms(),
        };
        check_target(ctx, Recipient::Group(group_id), target).await?;
        // NOTE: the admin list comes from Signal's group state, never from the
        // input. The store's mark_admin_deleted trusts its caller on this, so
        // skipping the check would let any member erase anyone's messages here.
        let me = ctx.signal.account().await?.service_id;
        let groups = ctx.signal.groups().await?;
        let Some(found) = groups.iter().find(|g| g.id == group_id) else {
            return Err(anyhow!("you aren't a member of group {group_id}").into());
        };
        if !found.admins.contains(&me) {
            return Err(anyhow!("you aren't an admin of \"{}\"", found.title).into());
        }
        ctx.signal
            .send(Recipient::Group(group_id), Outgoing::AdminDelete { target })
            .await?;
        ctx.store
            .mark_admin_deleted(group_id, target)
            .await
            .context("Signal accepted the delete, but the stored copy wasn't flagged")?;
        Ok(json!({ "status": "deleted" }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use relay_signal::{Account, FakeSignal, Group, Outgoing};
    use relay_store::MemoryStore;
    use serde_json::json;

    use crate::{Registry, ToolContext, testing::*};
    use relay_core::prelude::*;

    fn group() -> GroupId {
        GroupId::new([7; 32])
    }

    /// A full-access context in `group()`, an admin there when `admin` is true,
    /// holding a message from member 2 at timestamp 42.
    async fn in_group(admin: bool) -> (ToolContext, Arc<FakeSignal>) {
        let signal = Arc::new(
            FakeSignal::new(Account {
                service_id: sid(ME),
                phone_number: None,
                kind: ProfileKind::Linked,
                device_name: "test".into(),
            })
            .with_groups(vec![Group {
                id: group(),
                title: "band".into(),
                members: vec![sid(ME), sid(2), sid(3)],
                admins: if admin { vec![sid(ME)] } else { vec![sid(3)] },
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
        let message = Message {
            conversation: Recipient::Group(group()),
            ..incoming(2, 42, "spam")
        };
        ctx.store.insert(&message).await.unwrap();
        (ctx, signal)
    }

    fn delete_input() -> serde_json::Value {
        json!({
            "group_id": group().to_string(),
            "target_author": sid(2).to_string(),
            "target_timestamp": 42,
        })
    }

    /// An admin's delete goes to the group naming the member's message, and
    /// only then is the stored copy flagged deleted.
    #[tokio::test]
    async fn an_admins_delete_reaches_the_group_then_flags_the_stored_copy() {
        let (ctx, signal) = in_group(true).await;
        Registry::standard()
            .call(&ctx, "admin_delete_message", delete_input())
            .await
            .unwrap();
        let target = MessageId {
            author: sid(2),
            sent_at_ms: 42,
        };
        assert_eq!(
            signal.sent(),
            [(Recipient::Group(group()), Outgoing::AdminDelete { target })]
        );
        let stored = ctx.store.get(target).await.unwrap().unwrap();
        assert!(stored.deleted, "admin delete not flagged: {stored:?}");
    }

    /// Admin rights come from Signal's group roles, never the caller: a member
    /// who isn't an admin must be refused before anything is sent, and the
    /// stored message must stay as it was.
    #[tokio::test]
    async fn a_non_admins_admin_delete_is_refused_and_nothing_is_sent() {
        let (ctx, signal) = in_group(false).await;
        let result = Registry::standard()
            .call(&ctx, "admin_delete_message", delete_input())
            .await;
        assert!(result.is_err(), "non-admin delete succeeded: {result:?}");
        assert!(signal.sent().is_empty(), "sent {:?}", signal.sent());
        let target = MessageId {
            author: sid(2),
            sent_at_ms: 42,
        };
        let stored = ctx.store.get(target).await.unwrap().unwrap();
        assert!(!stored.deleted, "non-admin delete flagged {stored:?}");
    }
}
