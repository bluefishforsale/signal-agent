//! Tools that ask Signal about accounts, keys and devices. They reach Signal's
//! servers or the protocol store but never a person, so nothing here sends.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{contact, contact_schema, parse},
};

pub struct ListDevices;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListDevicesInput {}

#[async_trait]
impl Tool for ListDevices {
    fn name(&self) -> &'static str {
        "list_devices"
    }
    fn description(&self) -> &'static str {
        "List every device linked to this Signal account, as Signal's servers report them: id, \
         name, created_at_ms and last_seen_at_ms (epoch ms), and primary (id 1, the phone the \
         account was registered on). Use it to audit which devices can read the account. Takes \
         no parameters. Nothing is sent."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let ListDevicesInput {} = parse(input)?;
        let devices: Vec<_> = ctx
            .signal
            .devices()
            .await?
            .into_iter()
            .map(|d| {
                json!({
                    "id": d.id,
                    "name": d.name,
                    "created_at_ms": d.created_at_ms,
                    "last_seen_at_ms": d.last_seen_at_ms,
                    "primary": d.id == 1,
                })
            })
            .collect();
        Ok(json!({ "devices": devices }))
    }
}

pub struct ListIdentities;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListIdentitiesInput {
    number: Option<String>,
}

#[async_trait]
impl Tool for ListIdentities {
    fn name(&self) -> &'static str {
        "list_identities"
    }
    fn description(&self) -> &'static str {
        "List the identity keys stored for your contacts and how far each is trusted. changed \
         names every contact whose safety number changed and hasn't been verified since: tell \
         the user, because a changed number can mean a new phone or someone intercepting the \
         chat, and only comparing safety numbers in person tells which. identities lists each \
         key, changed ones first: service_id, number (null if unknown), safety_number, trust \
         (verified, unverified or untrusted) and changed. number limits the list to one \
         contact. Nothing is sent and no trust is changed."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"number": contact_schema("Only this contact's keys")},
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let ListIdentitiesInput { number } = parse(input)?;
        let only = match number {
            Some(n) => Some(contact(ctx, &n).await?),
            None => None,
        };
        let contacts = ctx.signal.contacts().await?;
        let mut identities: Vec<_> = ctx
            .signal
            .identities()
            .await?
            .into_iter()
            .filter(|i| only.is_none_or(|id| id == i.service_id))
            .collect();
        identities.sort_by_key(|i| !i.changed);
        let changed: Vec<_> = identities
            .iter()
            .filter(|i| i.changed)
            .map(|i| i.service_id.to_string())
            .collect();
        let identities: Vec<_> = identities
            .into_iter()
            .map(|i| {
                let number = contacts
                    .iter()
                    .find(|c| c.service_id == i.service_id)
                    .and_then(|c| c.phone_number.clone());
                json!({
                    "service_id": i.service_id.to_string(),
                    "number": number,
                    "safety_number": i.safety_number,
                    "trust": i.trust,
                    "changed": i.changed,
                })
            })
            .collect();
        Ok(json!({ "changed": changed, "identities": identities }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use relay_signal::{Account, Contact, Device, FakeSignal, Identity, Trust};
    use relay_store::MemoryStore;
    use serde_json::json;

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
            send_roots: Arc::default(),
            webhook: None,
        }
    }

    /// Auditing who can read the account is the point of the list, so every
    /// device comes back with its times, and the primary is marked.
    #[tokio::test]
    async fn list_devices_lists_every_device_and_marks_the_primary() {
        let ctx = ctx_of(|s| {
            s.with_devices(vec![
                Device {
                    id: 1,
                    name: Some("phone".into()),
                    created_at_ms: 10,
                    last_seen_at_ms: 20,
                },
                Device {
                    id: 2,
                    name: None,
                    created_at_ms: 30,
                    last_seen_at_ms: 40,
                },
            ])
        });
        let out = Registry::standard()
            .call(&ctx, "list_devices", json!({}))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({"devices": [
                {"id": 1, "name": "phone", "created_at_ms": 10, "last_seen_at_ms": 20, "primary": true},
                {"id": 2, "name": null, "created_at_ms": 30, "last_seen_at_ms": 40, "primary": false},
            ]})
        );
    }

    fn identity(n: u128, trust: Trust, changed: bool) -> Identity {
        Identity {
            service_id: sid(n),
            safety_number: format!("{n:060}"),
            trust,
            changed,
        }
    }

    /// A changed safety number is the one thing in this list that can mean
    /// someone is intercepting a chat, so changed keys come first and are
    /// named again at the top, where a client summarizing the result can't
    /// bury them.
    #[tokio::test]
    async fn list_identities_puts_changed_safety_numbers_first_and_names_them() {
        let ctx = ctx_of(|s| {
            s.with_contacts(vec![Contact {
                service_id: sid(3),
                name: Some("Grace".into()),
                phone_number: Some("+15550000003".into()),
            }])
            .with_identities(vec![
                identity(2, Trust::Verified, false),
                identity(3, Trust::Unverified, true),
            ])
        });
        let out = Registry::standard()
            .call(&ctx, "list_identities", json!({}))
            .await
            .unwrap();
        assert_eq!(out["changed"], json!([sid(3).to_string()]), "{out}");
        assert_eq!(
            out["identities"],
            json!([
                {
                    "service_id": sid(3).to_string(),
                    "number": "+15550000003",
                    "safety_number": format!("{:060}", 3),
                    "trust": "unverified",
                    "changed": true,
                },
                {
                    "service_id": sid(2).to_string(),
                    "number": null,
                    "safety_number": format!("{:060}", 2),
                    "trust": "verified",
                    "changed": false,
                },
            ]),
            "{out}"
        );
    }

    /// number narrows the list to that one contact's keys.
    #[tokio::test]
    async fn list_identities_with_a_number_returns_only_that_contacts_keys() {
        let ctx = ctx_of(|s| {
            s.with_contacts(vec![Contact {
                service_id: sid(3),
                name: None,
                phone_number: Some("+15550000003".into()),
            }])
            .with_identities(vec![
                identity(2, Trust::Verified, false),
                identity(3, Trust::Untrusted, false),
            ])
        });
        let out = Registry::standard()
            .call(&ctx, "list_identities", json!({"number": "+15550000003"}))
            .await
            .unwrap();
        let ids: Vec<_> = out["identities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["service_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [sid(3).to_string()], "{out}");
        assert_eq!(out["changed"], json!([]), "{out}");
    }
}
