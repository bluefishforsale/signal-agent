//! Tools that look up who and what the account knows: contacts, groups and
//! received attachments. They read from Signal's side of the seam or the local
//! store, and never send.

use anyhow::Context;
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use relay_core::prelude::*;
use relay_signal::Contact;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{Limit, Offset, group, group_schema, non_blank, parse},
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
         group. No messages sent."
    }
    fn input_schema(&self) -> Value {
        let mut group_id = group_schema();
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
        let only = group_id.as_deref().map(group).transpose()?;
        let groups: Vec<_> = ctx
            .signal
            .groups()
            .await?
            .iter()
            .filter(|g| only.is_none_or(|id| id == g.id))
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

pub struct ListAttachments;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListAttachmentsInput {
    #[serde(default)]
    limit: Limit,
    #[serde(default)]
    offset: Offset,
}

#[async_trait]
impl Tool for ListAttachments {
    fn name(&self) -> &'static str {
        "list_attachments"
    }
    fn description(&self) -> &'static str {
        "List the attachments in stored message history, newest first: id, content_type, \
         file_name, size (bytes), the message (author, sent_at_ms) and conversation it came \
         in. limit is the page size; offset skips the newest N to page back; has_more says \
         whether older ones remain. Pass an id to get_attachment for the file itself. Local \
         only; nothing is downloaded."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"limit": Limit::schema(), "offset": Offset::schema()},
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let ListAttachmentsInput { limit, offset } = parse(input)?;
        let limit = limit.get() as usize;
        let mut page: Vec<_> = stored_attachments(ctx)
            .await?
            .skip(offset.get() as usize)
            .take(limit + 1)
            .collect();
        let has_more = page.len() > limit;
        page.truncate(limit);
        let attachments: Vec<_> = page
            .into_iter()
            .map(|s| {
                json!({
                    "id": s.attachment.id,
                    "content_type": s.attachment.content_type,
                    "file_name": s.attachment.file_name,
                    "size": s.attachment.size,
                    "message": s.message,
                    "conversation": s.conversation,
                })
            })
            .collect();
        Ok(json!({ "attachments": attachments, "has_more": has_more }))
    }
}

pub struct GetAttachment;

/// The largest file get_attachment returns: 5 MiB, the most an image block in
/// the Claude API accepts, and already about 7 MB once base64'd.
const MAX_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetAttachmentInput {
    filename: String,
}

#[async_trait]
impl Tool for GetAttachment {
    fn name(&self) -> &'static str {
        "get_attachment"
    }
    fn description(&self) -> &'static str {
        "Fetch one received attachment: its id, content_type, file_name and size (bytes), and \
         the file itself as base64. filename is an attachment id from list_attachments or a \
         message's attachments. Files over 5 MiB are refused rather than returned. Nothing is \
         sent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filename": {
                    "type": "string",
                    "minLength": 1,
                    "description": "An attachment id, from list_attachments or a message's attachments",
                },
            },
            "required": ["filename"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let GetAttachmentInput { filename } = parse(input)?;
        let id = non_blank("filename", filename)?;
        let attachment = stored_attachments(ctx)
            .await?
            .map(|s| s.attachment)
            .find(|a| a.id == id)
            .with_context(|| {
                format!("no stored message has an attachment with id \"{id}\"; list_attachments shows them")
            })?;
        too_big(&id, attachment.size)?;
        let bytes = ctx.signal.attachment(&attachment).await?;
        too_big(&id, bytes.len() as u64)?;
        Ok(json!({
            "id": attachment.id,
            "content_type": attachment.content_type,
            "file_name": attachment.file_name,
            "size": attachment.size,
            "base64": STANDARD.encode(bytes),
        }))
    }
}

fn too_big(id: &str, size: u64) -> Result<(), ToolError> {
    if size <= MAX_ATTACHMENT_BYTES {
        return Ok(());
    }
    Err(ToolError::InvalidInput(format!(
        "attachment \"{id}\" is {size} bytes, over get_attachment's 5 MiB cap"
    )))
}

/// An attachment in stored history, with the message that carried it.
struct Stored {
    attachment: AttachmentRef,
    message: MessageId,
    conversation: Recipient,
}

/// Every attachment in stored history, newest message first, leaving out
/// messages deleted for everyone.
// NOTE: loads the whole store, since MessageStore has no attachment query.
// Fine at personal-history sizes; issue #65 proposes the paged query to replace it.
async fn stored_attachments(ctx: &ToolContext) -> anyhow::Result<impl Iterator<Item = Stored>> {
    let messages = ctx.store.export(None, None).await?;
    Ok(messages
        .into_iter()
        .rev()
        .filter(|m| !m.deleted)
        .flat_map(|m| {
            m.attachments.into_iter().map(move |attachment| Stored {
                attachment,
                message: m.id,
                conversation: m.conversation,
            })
        }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use relay_signal::{Account, Contact, FakeSignal, Group};
    use relay_store::MemoryStore;

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
            admins: vec![],
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

    fn attachment(id: &str, size: u64) -> AttachmentRef {
        AttachmentRef {
            id: id.into(),
            content_type: "image/jpeg".into(),
            file_name: Some(format!("{id}.jpg")),
            size,
        }
    }

    fn with_attachments(sender: u128, sent_at_ms: u64, ids: &[&str]) -> Message {
        Message {
            attachments: ids.iter().map(|id| attachment(id, 3)).collect(),
            ..incoming(sender, sent_at_ms, "look")
        }
    }

    /// Every attachment in stored history must be listed with the message it
    /// came in, newest first, so a client can find "the photo Ada just sent".
    #[tokio::test]
    async fn list_attachments_lists_stored_attachments_newest_first_with_their_message() {
        let ctx = ctx_of(|s| s);
        ctx.store
            .insert(&with_attachments(2, 100, &["old"]))
            .await
            .unwrap();
        ctx.store
            .insert(&incoming(2, 150, "no files"))
            .await
            .unwrap();
        ctx.store
            .insert(&with_attachments(3, 200, &["a", "b"]))
            .await
            .unwrap();
        let out = Registry::standard()
            .call(&ctx, "list_attachments", json!({}))
            .await
            .unwrap();
        let ids: Vec<_> = out["attachments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a", "b", "old"], "{out}");
        assert_eq!(
            out["attachments"][2],
            json!({
                "id": "old",
                "content_type": "image/jpeg",
                "file_name": "old.jpg",
                "size": 3,
                "message": {"author": sid(2).to_string(), "sent_at_ms": 100},
                "conversation": {"kind": "contact", "id": sid(2).to_string()},
            }),
        );
    }

    /// A message deleted for everyone takes its files with it: listing them
    /// would resurface what the sender retracted.
    #[tokio::test]
    async fn list_attachments_leaves_out_files_from_deleted_messages() {
        let ctx = ctx_of(|s| s);
        let gone = with_attachments(2, 100, &["retracted"]);
        ctx.store.insert(&gone).await.unwrap();
        ctx.store
            .insert(&with_attachments(2, 200, &["kept"]))
            .await
            .unwrap();
        ctx.store.mark_deleted(gone.id, sid(2)).await.unwrap();
        let out = Registry::standard()
            .call(&ctx, "list_attachments", json!({}))
            .await
            .unwrap();
        let ids: Vec<_> = out["attachments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["kept"], "{out}");
    }

    /// The file must come back byte for byte with its metadata, fetched by the
    /// id list_attachments gave.
    #[tokio::test]
    async fn get_attachment_returns_metadata_and_the_bytes_as_base64() {
        let ctx = ctx_of(|s| s.with_attachment("pic", vec![0xff, 0x00, 0xfe]));
        ctx.store
            .insert(&with_attachments(2, 100, &["pic"]))
            .await
            .unwrap();
        let out = Registry::standard()
            .call(&ctx, "get_attachment", json!({"filename": "pic"}))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({
                "id": "pic",
                "content_type": "image/jpeg",
                "file_name": "pic.jpg",
                "size": 3,
                "base64": "/wD+",
            })
        );
    }

    /// A 200 MB video base64'd into one MCP response would flood the client,
    /// so anything declared over the cap is refused before it's downloaded.
    #[tokio::test]
    async fn get_attachment_refuses_a_file_declared_over_the_cap() {
        let ctx = ctx_of(|s| s.with_attachment("video", vec![0; 3]));
        let mut message = with_attachments(2, 100, &["video"]);
        message.attachments[0].size = super::MAX_ATTACHMENT_BYTES + 1;
        ctx.store.insert(&message).await.unwrap();
        let result = Registry::standard()
            .call(&ctx, "get_attachment", json!({"filename": "video"}))
            .await;
        assert!(
            matches!(&result, Err(ToolError::InvalidInput(e)) if e.contains("5 MiB")),
            "got {result:?}"
        );
    }

    /// The declared size comes from the sender, so a file that claims to be
    /// small but isn't must still be refused once its real size is known.
    #[tokio::test]
    async fn get_attachment_refuses_a_file_whose_real_size_exceeds_the_cap() {
        let real = usize::try_from(super::MAX_ATTACHMENT_BYTES).unwrap() + 1;
        let ctx = ctx_of(|s| s.with_attachment("liar", vec![0; real]));
        ctx.store
            .insert(&with_attachments(2, 100, &["liar"]))
            .await
            .unwrap();
        let result = Registry::standard()
            .call(&ctx, "get_attachment", json!({"filename": "liar"}))
            .await;
        assert!(
            matches!(&result, Err(ToolError::InvalidInput(e)) if e.contains("5 MiB")),
            "got {:?}",
            result.map(|_| "a file over the cap")
        );
    }

    /// A person's id in group_id is a caller mistake; answering with no groups
    /// would read as "you're in no such group" instead of "wrong kind of id".
    #[tokio::test]
    async fn list_groups_refuses_a_persons_id_as_group_id() {
        let ctx = ctx_of(|s| s.with_groups(vec![group(0xaa, "Cats", &[ME, 2])]));
        let result = Registry::standard()
            .call(&ctx, "list_groups", json!({"group_id": sid(2).to_string()}))
            .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "got {result:?}"
        );
    }

    /// An unbounded list over a long history would be one enormous MCP
    /// response, so the list pages like get_conversation: exactly limit items,
    /// offset skipping the newest, and has_more while older ones remain.
    #[tokio::test]
    async fn list_attachments_pages_with_limit_offset_and_has_more() {
        let ctx = ctx_of(|s| s);
        for (t, id) in [(100, "a"), (200, "b"), (300, "c"), (400, "d")] {
            ctx.store
                .insert(&with_attachments(2, t, &[id]))
                .await
                .unwrap();
        }
        let registry = Registry::standard();
        let page = |input| registry.call(&ctx, "list_attachments", input);
        let ids = |out: &serde_json::Value| -> Vec<String> {
            out["attachments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["id"].as_str().unwrap().to_owned())
                .collect()
        };
        let first = page(json!({"limit": 2})).await.unwrap();
        assert_eq!(ids(&first), ["d", "c"], "{first}");
        assert_eq!(first["has_more"], json!(true), "{first}");
        let last = page(json!({"limit": 2, "offset": 2})).await.unwrap();
        assert_eq!(ids(&last), ["b", "a"], "{last}");
        assert_eq!(last["has_more"], json!(false), "{last}");
    }
}
