//! Tools that look up who and what the account knows: contacts, groups and
//! received attachments. They read from Signal's side of the seam or the local
//! store, and never send.

use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::Contact;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{non_blank, parse, recipient, recipient_schema},
};

pub struct ListContacts;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListContactsInput {
    search: Option<String>,
}

#[async_trait]
impl Tool for ListContacts {
    fn name(&self) -> &'static str {
        "list_contacts"
    }
    fn description(&self) -> &'static str {
        "List the contacts this account knows. search keeps only contacts whose name or phone \
         number contains it (case-insensitive). Returns number (E.164, null if unknown), \
         service_id and name for each. Pass a number or service_id to any tool that takes a \
         contact. No messages sent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "search": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Keep contacts whose name or number contains this (case-insensitive)",
                },
            },
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let ListContactsInput { search } = parse(input)?;
        let needle = search
            .map(|s| non_blank("search", s))
            .transpose()?
            .map(|s| s.to_lowercase());
        let contacts: Vec<_> = ctx
            .signal
            .contacts()
            .await?
            .iter()
            .filter(|c| needle.as_deref().is_none_or(|n| matches(c, n)))
            .map(|c| {
                json!({
                    "number": c.phone_number,
                    "service_id": c.service_id.to_string(),
                    "name": c.name,
                })
            })
            .collect();
        Ok(json!({ "contacts": contacts }))
    }
}

fn matches(contact: &Contact, needle: &str) -> bool {
    [&contact.name, &contact.phone_number]
        .into_iter()
        .flatten()
        .any(|field| field.to_lowercase().contains(needle))
}

pub struct ListGroups;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListGroupsInput {
    group_id: Option<String>,
}

#[async_trait]
impl Tool for ListGroups {
    fn name(&self) -> &'static str {
        "list_groups"
    }
    fn description(&self) -> &'static str {
        "List the Signal groups this account is a member of: id (pass it as the recipient of \
         group tools), name, member_count and members (service ids). group_id returns only that \
         group; a contact's id matches no group. No messages sent."
    }
    fn input_schema(&self) -> Value {
        let mut group_id = recipient_schema();
        group_id["description"] = json!("Return only this group (64 hex characters)");
        json!({
            "type": "object",
            "properties": {"group_id": group_id},
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let ListGroupsInput { group_id } = parse(input)?;
        let only = match group_id {
            Some(s) => Some(recipient(ctx, &s).await?),
            None => None,
        };
        let groups: Vec<_> = ctx
            .signal
            .groups()
            .await?
            .iter()
            .filter(|g| only.is_none_or(|r| r == Recipient::Group(g.id)))
            .map(|g| {
                json!({
                    "id": g.id.to_string(),
                    "name": g.title,
                    "member_count": g.members.len(),
                    "members": g.members.iter().map(ToString::to_string).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(json!({ "groups": groups }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use relay_signal::{Account, Contact, FakeSignal, Group};
    use relay_store::MemoryStore;

    use crate::{Registry, ToolContext, testing::*};
    use relay_core::prelude::*;

    /// A read-only linked profile whose Signal knows `signal`'s seeded state.
    fn ctx_of(signal: impl FnOnce(FakeSignal) -> FakeSignal) -> ToolContext {
        let fake = FakeSignal::new(Account {
            service_id: sid(ME),
            phone_number: Some("+15550000001".into()),
            kind: ProfileKind::Linked,
            device_name: "test".into(),
        });
        ToolContext {
            store: Arc::new(MemoryStore::default()),
            signal: Arc::new(signal(fake)),
            access: AccessMode::ReadOnly,
            kind: ProfileKind::Linked,
        }
    }

    fn contact(n: u128, name: &str, number: &str) -> Contact {
        Contact {
            service_id: sid(n),
            name: Some(name.into()),
            phone_number: Some(number.into()),
        }
    }

    /// Clients resolve "text Ada" through search, so it must match a name or a
    /// number fragment regardless of case, and leave everyone else out.
    #[tokio::test]
    async fn list_contacts_search_matches_name_or_number_ignoring_case() {
        let contacts = vec![
            contact(2, "Ada Lovelace", "+15550000002"),
            contact(3, "Grace Hopper", "+15550000003"),
            contact(4, "Alan Turing", "+15559990004"),
        ];
        let (ctx, _) = ctx_with(AccessMode::ReadOnly, ProfileKind::Linked, contacts);
        let registry = Registry::standard();
        let names = |out: serde_json::Value| -> Vec<String> {
            out["contacts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["name"].as_str().unwrap().to_owned())
                .collect()
        };
        let by_name = registry
            .call(&ctx, "list_contacts", json!({"search": "LOVE"}))
            .await
            .unwrap();
        assert_eq!(names(by_name), ["Ada Lovelace"]);
        let by_number = registry
            .call(&ctx, "list_contacts", json!({"search": "999"}))
            .await
            .unwrap();
        assert_eq!(names(by_number), ["Alan Turing"]);
        let everyone = registry
            .call(&ctx, "list_contacts", json!({}))
            .await
            .unwrap();
        assert_eq!(
            everyone["contacts"].as_array().unwrap().len(),
            3,
            "{everyone}"
        );
    }

    fn group(byte: u8, title: &str, members: &[u128]) -> Group {
        Group {
            id: GroupId::new([byte; 32]),
            title: title.into(),
            members: members.iter().map(|&n| sid(n)).collect(),
        }
    }

    /// group_id must narrow the list to exactly that group, since clients call
    /// it to check one group's members before sending to it.
    #[tokio::test]
    async fn list_groups_with_a_group_id_returns_only_that_group() {
        let ctx = ctx_of(|s| {
            s.with_groups(vec![
                group(0xaa, "Cats", &[ME, 2]),
                group(0xbb, "Techno", &[ME, 2, 3]),
            ])
        });
        let registry = Registry::standard();
        let all = registry.call(&ctx, "list_groups", json!({})).await.unwrap();
        assert_eq!(all["groups"].as_array().unwrap().len(), 2, "{all}");
        let one = registry
            .call(&ctx, "list_groups", json!({"group_id": "bb".repeat(32)}))
            .await
            .unwrap();
        assert_eq!(
            one,
            json!({"groups": [{
                "id": "bb".repeat(32),
                "name": "Techno",
                "member_count": 3,
                "members": [sid(ME).to_string(), sid(2).to_string(), sid(3).to_string()],
            }]})
        );
    }
}
