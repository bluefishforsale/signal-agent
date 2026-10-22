//! Tools that ask Signal about accounts, keys and devices. They reach Signal's
//! servers or the protocol store but never a person, so nothing here sends.

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    directory::MAX_ATTACHMENT_BYTES,
    input::{
        contact, contact_schema, e164, e164_schema, non_blank, parse, recipient, recipient_schema,
    },
};

pub struct GetProfile;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetProfileInput {
    number: String,
}

#[async_trait]
impl Tool for GetProfile {
    fn name(&self) -> &'static str {
        "get_profile"
    }
    fn description(&self) -> &'static str {
        "Check one phone number against Signal's servers. number is E.164 and need not be a \
         contact. Returns number, service_id (the Signal account's id, null when the number \
         isn't registered or hides itself from discovery) and registered. The lookup carries \
         no profile name; list_contacts has names for contacts and get_avatar their picture. \
         Use get_user_status to check several numbers or usernames at once. Nothing is sent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"number": e164_schema("The number to look up")},
            "required": ["number"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let GetProfileInput { number } = parse(input)?;
        let number = e164(number)?;
        let found = ctx.signal.discover(std::slice::from_ref(&number)).await?;
        let service_id = found
            .into_iter()
            .find_map(|(n, id)| (n == number).then_some(id))
            .flatten();
        Ok(json!({
            "number": number,
            "service_id": service_id.map(|id| id.to_string()),
            "registered": service_id.is_some(),
        }))
    }
}

pub struct GetUserStatus;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetUserStatusInput {
    recipients: Option<Vec<String>>,
    usernames: Option<Vec<String>>,
}

impl GetUserStatus {
    // NOTE: discovery reaches Signal's servers, which rate-limit it per
    // account. Without a cap one looping client could spend that budget in a
    // few calls and get the account's lookups throttled.
    const MAX_LOOKUPS: usize = 50;

    fn list_schema(items: Value, description: &str) -> Value {
        json!({
            "type": "array",
            "items": items,
            "minItems": 1,
            "maxItems": Self::MAX_LOOKUPS,
            "description": description,
        })
    }

    /// `list` when given, which must hold 1 to MAX_LOOKUPS entries.
    fn checked(field: &str, list: Option<Vec<String>>) -> Result<Vec<String>, ToolError> {
        let Some(list) = list else {
            return Ok(vec![]);
        };
        if list.is_empty() || list.len() > Self::MAX_LOOKUPS {
            return Err(ToolError::InvalidInput(format!(
                "{field} holds 1 to {} entries, got {}",
                Self::MAX_LOOKUPS,
                list.len()
            )));
        }
        Ok(list)
    }
}

#[async_trait]
impl Tool for GetUserStatus {
    fn name(&self) -> &'static str {
        "get_user_status"
    }
    fn description(&self) -> &'static str {
        "Check whether phone numbers or Signal usernames are registered on Signal, in one \
         batch. recipients are E.164 numbers and usernames are Signal usernames (alice.42); \
         give at least one of the two, each up to 50. Returns results, one per input in the \
         order given, numbers first: number or username, service_id (null when not \
         registered) and registered. A number that hides itself from discovery shows as not \
         registered. Nothing is sent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipients": Self::list_schema(e164_schema("A number to check"), "Phone numbers to check"),
                "usernames": Self::list_schema(
                    json!({"type": "string", "minLength": 1, "description": "A Signal username, e.g. alice.42"}),
                    "Signal usernames to check",
                ),
            },
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let GetUserStatusInput {
            recipients,
            usernames,
        } = parse(input)?;
        if recipients.is_none() && usernames.is_none() {
            return Err(ToolError::InvalidInput(
                "give recipients, usernames or both".into(),
            ));
        }
        let numbers = Self::checked("recipients", recipients)?
            .into_iter()
            .map(e164)
            .collect::<Result<Vec<_>, _>>()?;
        let usernames = Self::checked("usernames", usernames)?
            .into_iter()
            .map(|u| non_blank("usernames", u))
            .collect::<Result<Vec<_>, _>>()?;
        let mut results = vec![];
        if !numbers.is_empty() {
            for (number, id) in ctx.signal.discover(&numbers).await? {
                results.push(json!({
                    "number": number,
                    "service_id": id.map(|id| id.to_string()),
                    "registered": id.is_some(),
                }));
            }
        }
        for username in usernames {
            let id = ctx.signal.lookup_username(&username).await?;
            results.push(json!({
                "username": username,
                "service_id": id.map(|id| id.to_string()),
                "registered": id.is_some(),
            }));
        }
        Ok(json!({ "results": results }))
    }
}

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

pub struct GetAvatar;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetAvatarInput {
    identifier: String,
}

#[async_trait]
impl Tool for GetAvatar {
    fn name(&self) -> &'static str {
        "get_avatar"
    }
    fn description(&self) -> &'static str {
        "Fetch a contact's profile picture or a group's avatar. identifier is a contact's phone \
         number or service id, or a group id from list_groups. Returns identifier, has_avatar, \
         size (bytes) and the image as base64 (JPEG or PNG), or has_avatar false and base64 \
         null when there is none. Images over 5 MiB are refused rather than returned. Nothing \
         is sent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"identifier": recipient_schema()},
            "required": ["identifier"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let GetAvatarInput { identifier } = parse(input)?;
        let of = recipient(ctx, &identifier).await?;
        let Some(bytes) = ctx.signal.avatar(of).await? else {
            return Ok(json!({
                "identifier": identifier,
                "has_avatar": false,
                "size": null,
                "base64": null,
            }));
        };
        let size = bytes.len() as u64;
        if size > MAX_ATTACHMENT_BYTES {
            return Err(ToolError::InvalidInput(format!(
                "the avatar of \"{identifier}\" is {size} bytes, over the 5 MiB cap"
            )));
        }
        Ok(json!({
            "identifier": identifier,
            "has_avatar": true,
            "size": size,
            "base64": STANDARD.encode(bytes),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use relay_signal::{Account, Contact, Device, FakeSignal, Identity, Trust};
    use relay_store::MemoryStore;
    use serde_json::json;

    use crate::{Registry, ToolContext, ToolError, testing::*};
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

    /// A group id names the group's avatar, not a member's, and someone with
    /// no picture is an answer, not an error.
    #[tokio::test]
    async fn get_avatar_returns_a_groups_image_and_reports_a_missing_one() {
        let group = GroupId::new([0xaa; 32]);
        let ctx = ctx_of(|s| s.with_avatar(Recipient::Group(group), vec![0xff, 0x00, 0xfe]));
        let registry = Registry::standard();
        let out = registry
            .call(&ctx, "get_avatar", json!({"identifier": group.to_string()}))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({"identifier": group.to_string(), "has_avatar": true, "size": 3, "base64": "/wD+"})
        );
        let none = registry
            .call(
                &ctx,
                "get_avatar",
                json!({"identifier": sid(2).to_string()}),
            )
            .await
            .unwrap();
        assert_eq!(none["has_avatar"], json!(false), "{none}");
        assert_eq!(none["base64"], json!(null), "{none}");
    }

    /// An oversized avatar base64'd into one response would flood the client
    /// the same way a big attachment would, so get_attachment's cap applies.
    #[tokio::test]
    async fn get_avatar_refuses_an_image_over_the_attachment_cap() {
        let big = usize::try_from(crate::directory::MAX_ATTACHMENT_BYTES).unwrap() + 1;
        let ctx = ctx_of(|s| s.with_avatar(Recipient::Contact(sid(2)), vec![0; big]));
        let result = Registry::standard()
            .call(
                &ctx,
                "get_avatar",
                json!({"identifier": sid(2).to_string()}),
            )
            .await;
        assert!(
            matches!(&result, Err(ToolError::InvalidInput(e)) if e.contains("5 MiB")),
            "got {:?}",
            result.map(|_| "an avatar over the cap")
        );
    }

    /// The point of get_profile is a number nobody in the contacts has yet,
    /// so it must ask Signal rather than refuse a stranger's number.
    #[tokio::test]
    async fn get_profile_looks_up_a_number_that_is_not_a_contact() {
        let ctx = ctx_of(|s| s.with_registered("+15550000009", sid(9)));
        let registry = Registry::standard();
        let found = registry
            .call(&ctx, "get_profile", json!({"number": "+15550000009"}))
            .await
            .unwrap();
        assert_eq!(
            found,
            json!({"number": "+15550000009", "service_id": sid(9).to_string(), "registered": true})
        );
        let missing = registry
            .call(&ctx, "get_profile", json!({"number": "+15550000008"}))
            .await
            .unwrap();
        assert_eq!(
            missing,
            json!({"number": "+15550000008", "service_id": null, "registered": false})
        );
    }

    /// One batch answers every number and username, in the order given, so a
    /// client can line the answers up with what it asked.
    #[tokio::test]
    async fn get_user_status_answers_every_number_and_username_in_order() {
        let ctx = ctx_of(|s| {
            s.with_registered("+15550000009", sid(9))
                .with_username("ada.42", sid(4))
        });
        let out = Registry::standard()
            .call(
                &ctx,
                "get_user_status",
                json!({
                    "recipients": ["+15550000008", "+15550000009"],
                    "usernames": ["ada.42", "nobody.01"],
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({"results": [
                {"number": "+15550000008", "service_id": null, "registered": false},
                {"number": "+15550000009", "service_id": sid(9).to_string(), "registered": true},
                {"username": "ada.42", "service_id": sid(4).to_string(), "registered": true},
                {"username": "nobody.01", "service_id": null, "registered": false},
            ]})
        );
    }

    /// With neither list there is nothing to ask, and an empty answer would
    /// read as "nobody is registered".
    #[tokio::test]
    async fn get_user_status_with_neither_list_is_invalid_input() {
        let ctx = ctx_of(|s| s);
        let result = Registry::standard()
            .call(&ctx, "get_user_status", json!({}))
            .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "got {result:?}"
        );
    }
}
