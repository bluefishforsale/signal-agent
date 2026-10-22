//! Tools that send files and stickers. Files leave the machine only through
//! `SendRoots::admit`, so a prompt-injected client can't mail out a file the
//! user never put in a send folder.

use std::path::Path;

use anyhow::bail;
use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::{Outgoing, SendRoots};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{
        Timestamp, contact, contact_schema, group, group_schema, non_blank, parse, sticker_pack,
        sticker_pack_schema,
    },
    send::{quote, send_text},
};

/// The most files one call sends.
const MAX_FILES: usize = 32;

pub struct SendAttachment;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendAttachmentInput {
    recipient: String,
    path: Option<String>,
    paths: Option<Vec<String>>,
    caption: Option<String>,
    quote_author: Option<String>,
    quote_timestamp: Option<Timestamp>,
}

#[async_trait]
impl Tool for SendAttachment {
    fn name(&self) -> &'static str {
        "send_attachment"
    }
    fn description(&self) -> &'static str {
        "Send one or more local files (photos, videos, documents, audio) to one Signal contact \
         in a single message. Use send_group_attachment for groups and send_message for text \
         only. recipient is the contact's phone number or service id. path is one file, paths \
         several sent together (at least one file, at most 32). Files must be inside a send \
         folder the user chose under Advanced settings; with none chosen, no file can be sent, \
         and anything outside them is refused before any file is uploaded. caption is text \
         shown with the files. quote_author and quote_timestamp reply to a message. Not \
         idempotent: repeating sends a duplicate. Saved to the local store. Returns the new \
         message's id."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": contact_schema("Who to send to"),
                "path": path_schema(),
                "paths": paths_schema(),
                "caption": caption_schema(),
                "quote_author": contact_schema("Author of the message being replied to"),
                "quote_timestamp": Timestamp::schema("Send time of the message being replied to"),
            },
            "required": ["recipient"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SendAttachmentInput = parse(input)?;
        let to = Recipient::Contact(contact(ctx, &input.recipient).await?);
        let caption = input.caption.map(|c| non_blank("caption", c)).transpose()?;
        let quote = quote(ctx, input.quote_author, input.quote_timestamp).await?;
        let paths = files(input.path, input.paths)?;
        let attachments = upload(ctx, &paths).await?;
        send_text(ctx, to, caption.unwrap_or_default(), quote, attachments).await
    }
}

pub struct SendGroupAttachment;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendGroupAttachmentInput {
    group_id: String,
    path: Option<String>,
    paths: Option<Vec<String>>,
    caption: Option<String>,
    quote_author: Option<String>,
    quote_timestamp: Option<Timestamp>,
}

#[async_trait]
impl Tool for SendGroupAttachment {
    fn name(&self) -> &'static str {
        "send_group_attachment"
    }
    fn description(&self) -> &'static str {
        "Send one or more local files (photos, videos, documents, audio) to a Signal group you \
         belong to, in a single message. Use send_attachment for one contact and \
         send_group_message for text only. group_id comes from list_groups. path is one file, \
         paths several sent together (at least one file, at most 32). Files must be inside a \
         send folder the user chose under Advanced settings; with none chosen, no file can be \
         sent, and anything outside them is refused before any file is uploaded. caption is \
         text shown with the files. quote_author and quote_timestamp reply to a message. Not \
         idempotent: repeating sends a duplicate. Saved to the local store. Returns the new \
         message's id."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "group_id": group_schema(),
                "path": path_schema(),
                "paths": paths_schema(),
                "caption": caption_schema(),
                "quote_author": contact_schema("Author of the message being replied to"),
                "quote_timestamp": Timestamp::schema("Send time of the message being replied to"),
            },
            "required": ["group_id"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SendGroupAttachmentInput = parse(input)?;
        let to = Recipient::Group(group(&input.group_id)?);
        let caption = input.caption.map(|c| non_blank("caption", c)).transpose()?;
        let quote = quote(ctx, input.quote_author, input.quote_timestamp).await?;
        let paths = files(input.path, input.paths)?;
        let attachments = upload(ctx, &paths).await?;
        send_text(ctx, to, caption.unwrap_or_default(), quote, attachments).await
    }
}

pub struct SendSticker;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendStickerInput {
    recipient: String,
    pack_id: String,
    sticker_id: u32,
}

#[async_trait]
impl Tool for SendSticker {
    fn name(&self) -> &'static str {
        "send_sticker"
    }
    fn description(&self) -> &'static str {
        "Send one sticker from a sticker pack installed on this account to one Signal contact; \
         it shows as a sticker, not a file. Use send_group_sticker for groups and \
         send_attachment for ordinary images. recipient is the contact's phone number or \
         service id. pack_id is the pack's id (32 hex characters) and sticker_id the sticker's \
         number within it; a pack that isn't installed, or a sticker it doesn't hold, is an \
         error. Not idempotent: repeating sends a duplicate. Saved to the local store as \
         [sticker pack_id:sticker_id]. Returns the new message's id."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": contact_schema("Who to send to"),
                "pack_id": sticker_pack_schema(),
                "sticker_id": sticker_id_schema(),
            },
            "required": ["recipient", "pack_id", "sticker_id"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SendStickerInput = parse(input)?;
        let to = Recipient::Contact(contact(ctx, &input.recipient).await?);
        let pack = sticker_pack(&input.pack_id)?;
        send_sticker(ctx, to, pack, input.sticker_id).await
    }
}

pub struct SendGroupSticker;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendGroupStickerInput {
    group_id: String,
    pack_id: String,
    sticker_id: u32,
}

#[async_trait]
impl Tool for SendGroupSticker {
    fn name(&self) -> &'static str {
        "send_group_sticker"
    }
    fn description(&self) -> &'static str {
        "Send one sticker from a sticker pack installed on this account to a Signal group you \
         belong to; it shows as a sticker, not a file. Use send_sticker for one contact and \
         send_group_attachment for ordinary images. group_id comes from list_groups. pack_id \
         is the pack's id (32 hex characters) and sticker_id the sticker's number within it; a \
         pack that isn't installed, or a sticker it doesn't hold, is an error. Not idempotent: \
         repeating sends a duplicate. Saved to the local store as \
         [sticker pack_id:sticker_id]. Returns the new message's id."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "group_id": group_schema(),
                "pack_id": sticker_pack_schema(),
                "sticker_id": sticker_id_schema(),
            },
            "required": ["group_id", "pack_id", "sticker_id"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SendGroupStickerInput = parse(input)?;
        let to = Recipient::Group(group(&input.group_id)?);
        let pack = sticker_pack(&input.pack_id)?;
        send_sticker(ctx, to, pack, input.sticker_id).await
    }
}

fn sticker_id_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": u32::MAX,
        "description": "The sticker's number within its pack",
    })
}

/// Sends one sticker and records it in the local history, as send_text does
/// for text.
async fn send_sticker(
    ctx: &ToolContext,
    to: Recipient,
    pack: StickerPackId,
    sticker_id: u32,
) -> Result<Value, ToolError> {
    let id = ctx
        .signal
        .send(to, Outgoing::Sticker { pack, sticker_id })
        .await?;
    ctx.store
        .insert(&Message {
            id,
            conversation: to,
            sender: id.author,
            from_me: true,
            received_at_ms: id.sent_at_ms,
            body: Some(format!("[sticker {pack}:{sticker_id}]")),
            attachments: vec![],
            quote: None,
            edited: false,
            deleted: false,
        })
        .await?;
    Ok(json!({ "id": id }))
}

fn caption_schema() -> Value {
    json!({"type": "string", "minLength": 1, "description": "Text shown with the files"})
}

fn path_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "description": "A file to send: an absolute path inside a send folder",
    })
}

pub(crate) fn paths_schema() -> Value {
    json!({
        "type": "array",
        "items": path_schema(),
        "minItems": 1,
        "maxItems": MAX_FILES,
        "description": "Files to send together in one message",
    })
}

/// The files named by `path` and `paths` together: at least one, at most
/// MAX_FILES, none blank.
pub(crate) fn files(
    path: Option<String>,
    paths: Option<Vec<String>>,
) -> Result<Vec<String>, ToolError> {
    if paths.as_ref().is_some_and(Vec::is_empty) {
        return Err(ToolError::InvalidInput(
            "a file list must name at least one file".into(),
        ));
    }
    let all: Vec<_> = path
        .into_iter()
        .chain(paths.into_iter().flatten())
        .collect();
    if !(1..=MAX_FILES).contains(&all.len()) {
        return Err(ToolError::InvalidInput(format!(
            "send 1 to {MAX_FILES} files at a time, got {}",
            all.len()
        )));
    }
    all.into_iter().map(|p| non_blank("path", p)).collect()
}

/// Uploads `paths` for one message, admitting every file before uploading
/// any, so one refused path means nothing leaves the machine.
// NOTE: a refusal here is not InvalidInput. The call is well-formed; what's
// refused depends on the user's send folders and the disk, like a limiter
// refusal, and the message says what the user has to change.
pub(crate) async fn upload(
    ctx: &ToolContext,
    paths: &[String],
) -> Result<Vec<AttachmentRef>, ToolError> {
    let files = paths
        .iter()
        .map(|p| admit(&ctx.send_roots, Path::new(p)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut attachments = Vec::with_capacity(files.len());
    for file in &files {
        attachments.push(ctx.signal.upload(file).await?);
    }
    Ok(attachments)
}

fn admit(roots: &SendRoots, path: &Path) -> anyhow::Result<relay_signal::SendableFile> {
    if *roots == SendRoots::default() {
        bail!(
            "no folder is set up for sending files, so \"{}\" can't be sent; the user must \
             choose a send folder under Advanced settings first",
            path.display()
        );
    }
    roots.admit(path)
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use relay_signal::{Account, FakeSignal, Group, Outgoing, SendRoots, SignalBackend};
    use relay_store::MemoryStore;
    use serde_json::json;

    use crate::{Registry, ToolContext, testing::*};
    use relay_core::prelude::*;

    /// A temp folder holding `outbox/cat.png` and a `secret` beside it.
    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("outbox")).expect("outbox");
        std::fs::write(dir.path().join("outbox/cat.png"), b"png").expect("file");
        std::fs::write(dir.path().join("secret"), b"ssh key").expect("file");
        dir
    }

    /// A full-access profile that may send from `roots`.
    fn ctx_with_roots(roots: &[&Path]) -> (ToolContext, Arc<FakeSignal>) {
        let signal = Arc::new(FakeSignal::new(Account {
            service_id: sid(ME),
            phone_number: Some("+15550000001".into()),
            kind: ProfileKind::Primary,
            device_name: "test".into(),
        }));
        let ctx = ToolContext {
            store: Arc::new(MemoryStore::default()),
            signal: signal.clone(),
            access: AccessMode::Full,
            kind: ProfileKind::Primary,
            send_roots: Arc::new(
                SendRoots::new(roots.iter().map(|r| r.to_path_buf())).expect("roots"),
            ),
            webhook: None,
        };
        (ctx, signal)
    }

    /// A file in a send folder goes out as one message carrying the uploaded
    /// ref, and is stored with that ref so get_attachment can fetch it back.
    #[tokio::test]
    async fn send_attachment_uploads_a_file_in_a_send_root_and_stores_the_ref() {
        let dir = tree();
        let (ctx, signal) = ctx_with_roots(&[&dir.path().join("outbox")]);
        let path = dir.path().join("outbox/cat.png");
        let out = Registry::standard()
            .call(
                &ctx,
                "send_attachment",
                json!({
                    "recipient": sid(2).to_string(),
                    "path": path.to_str().unwrap(),
                    "caption": "the cat",
                }),
            )
            .await
            .unwrap();
        let sent = signal.sent();
        let [
            (
                to,
                Outgoing::Text {
                    body, attachments, ..
                },
            ),
        ] = sent.as_slice()
        else {
            panic!("expected one text with files, sent {sent:?}");
        };
        assert_eq!(*to, Recipient::Contact(sid(2)));
        assert_eq!(body, "the cat");
        assert_eq!(attachments.len(), 1, "{attachments:?}");
        assert_eq!(signal.attachment(&attachments[0]).await.unwrap(), b"png");
        let stored = ctx
            .store
            .attachment(&attachments[0].id)
            .await
            .unwrap()
            .expect("sent attachment stored");
        assert_eq!(json!(stored.message), out["id"], "{out}");
    }

    /// Asserts `input` to send_attachment was refused with a message
    /// containing `why`, and that no file was uploaded and nothing sent.
    async fn refused(ctx: &ToolContext, signal: &FakeSignal, input: serde_json::Value, why: &str) {
        refused_by(ctx, signal, "send_attachment", input, why).await;
    }

    /// Asserts `tool` refused `input` with a message containing `why`, and
    /// that no file was uploaded and nothing sent.
    async fn refused_by(
        ctx: &ToolContext,
        signal: &FakeSignal,
        tool: &str,
        input: serde_json::Value,
        why: &str,
    ) {
        let result = Registry::standard().call(ctx, tool, input.clone()).await;
        assert!(
            matches!(&result, Err(e) if e.to_string().contains(why)),
            "{input} was not refused for \"{why}\": {result:?}"
        );
        let first_upload = AttachmentRef {
            id: "upload-1".into(),
            content_type: String::new(),
            file_name: None,
            size: 0,
        };
        assert!(
            signal.attachment(&first_upload).await.is_err(),
            "{input} uploaded a file before being refused"
        );
        assert!(
            signal.sent().is_empty(),
            "{input} still sent: {:?}",
            signal.sent()
        );
    }

    /// One file outside the send folders refuses the whole call, before the
    /// allowed file beside it is uploaded: a half-sent message would still
    /// leak, and an upload already hands the bytes to Signal's servers.
    #[tokio::test]
    async fn a_path_outside_the_send_roots_refuses_the_call_before_any_upload() {
        let dir = tree();
        let (ctx, signal) = ctx_with_roots(&[&dir.path().join("outbox")]);
        let input = json!({
            "recipient": sid(2).to_string(),
            "paths": [
                dir.path().join("outbox/cat.png").to_str().unwrap(),
                dir.path().join("secret").to_str().unwrap(),
            ],
        });
        refused(&ctx, &signal, input, "outside the folders").await;
    }

    /// A `..` that climbs out of a send folder is judged by where it lands.
    #[tokio::test]
    async fn a_dot_dot_escape_from_a_send_root_is_refused() {
        let dir = tree();
        let (ctx, signal) = ctx_with_roots(&[&dir.path().join("outbox")]);
        let escape = dir.path().join("outbox/../secret");
        let input = json!({"recipient": sid(2).to_string(), "path": escape.to_str().unwrap()});
        refused(&ctx, &signal, input, "outside the folders").await;
    }

    /// A symlink inside a send folder is judged by its target, or linking the
    /// key into the outbox would send it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_escape_from_a_send_root_is_refused() {
        let dir = tree();
        let link = dir.path().join("outbox/innocent.png");
        std::os::unix::fs::symlink(dir.path().join("secret"), &link).expect("symlink");
        let (ctx, signal) = ctx_with_roots(&[&dir.path().join("outbox")]);
        let input = json!({"recipient": sid(2).to_string(), "path": link.to_str().unwrap()});
        refused(&ctx, &signal, input, "outside the folders").await;
    }

    /// Until the user picks a send folder no file is sendable, and the
    /// refusal must say that is what's missing, not just that the path is bad.
    #[tokio::test]
    async fn with_no_send_folder_chosen_the_refusal_says_to_choose_one() {
        let dir = tree();
        let (ctx, signal) = ctx_with_roots(&[]);
        let path = dir.path().join("outbox/cat.png");
        let input = json!({"recipient": sid(2).to_string(), "path": path.to_str().unwrap()});
        refused(&ctx, &signal, input, "choose a send folder under Advanced").await;
    }

    /// A group send carries the files to the group itself, not to a member.
    #[tokio::test]
    async fn send_group_attachment_sends_the_files_to_the_group() {
        let dir = tree();
        let group = GroupId::new([7; 32]);
        let (ctx, _) = ctx_with_roots(&[&dir.path().join("outbox")]);
        let signal = Arc::new(
            FakeSignal::new(ctx.signal.account().await.unwrap()).with_groups(vec![Group {
                id: group,
                title: "band".into(),
                members: vec![sid(ME), sid(2)],
                admins: vec![],
            }]),
        );
        let ctx = ToolContext {
            signal: signal.clone(),
            ..ctx
        };
        let path = dir.path().join("outbox/cat.png");
        Registry::standard()
            .call(
                &ctx,
                "send_group_attachment",
                json!({"group_id": group.to_string(), "paths": [path.to_str().unwrap()]}),
            )
            .await
            .unwrap();
        let sent = signal.sent();
        let [(to, Outgoing::Text { attachments, .. })] = sent.as_slice() else {
            panic!("expected one text with files, sent {sent:?}");
        };
        assert_eq!(*to, Recipient::Group(group));
        assert_eq!(attachments.len(), 1, "{attachments:?}");
    }

    /// Note to Self takes files too, through the same send folders: it syncs
    /// to every linked device, so it is still a way off this machine.
    #[tokio::test]
    async fn send_note_to_self_attaches_files_only_from_a_send_root() {
        let dir = tree();
        let (ctx, signal) = ctx_with_roots(&[&dir.path().join("outbox")]);
        let secret = dir.path().join("secret");
        let input = json!({"message": "keys", "attachments": [secret.to_str().unwrap()]});
        refused_by(
            &ctx,
            &signal,
            "send_note_to_self",
            input,
            "outside the folders",
        )
        .await;

        let cat = dir.path().join("outbox/cat.png");
        Registry::standard()
            .call(
                &ctx,
                "send_note_to_self",
                json!({"message": "cat", "attachments": [cat.to_str().unwrap()]}),
            )
            .await
            .unwrap();
        let sent = signal.sent();
        let [(to, Outgoing::Text { attachments, .. })] = sent.as_slice() else {
            panic!("expected one note with files, sent {sent:?}");
        };
        assert_eq!(*to, Recipient::Contact(sid(ME)));
        assert_eq!(attachments.len(), 1, "{attachments:?}");
    }

    /// A sticker from an installed pack goes out as a sticker, not a file.
    #[tokio::test]
    async fn send_sticker_sends_an_installed_sticker_to_one_contact() {
        let pack = StickerPackId::new([0xab; 16]);
        let signal = Arc::new(
            FakeSignal::new(Account {
                service_id: sid(ME),
                phone_number: Some("+15550000001".into()),
                kind: ProfileKind::Primary,
                device_name: "test".into(),
            })
            .with_sticker_pack(pack, vec![0, 5]),
        );
        let (ctx, _) = ctx_with_roots(&[]);
        let ctx = ToolContext {
            signal: signal.clone(),
            ..ctx
        };
        Registry::standard()
            .call(
                &ctx,
                "send_sticker",
                json!({"recipient": sid(2).to_string(), "pack_id": pack.to_string(), "sticker_id": 5}),
            )
            .await
            .unwrap();
        assert_eq!(
            signal.sent(),
            [(
                Recipient::Contact(sid(2)),
                Outgoing::Sticker {
                    pack,
                    sticker_id: 5
                }
            )]
        );
    }

    /// A group sticker goes to the group, and a pack that isn't installed is
    /// refused rather than sent as something Signal's apps can't render.
    #[tokio::test]
    async fn send_group_sticker_sends_to_the_group_and_refuses_an_uninstalled_pack() {
        let (pack, group) = (StickerPackId::new([0xab; 16]), GroupId::new([7; 32]));
        let signal = Arc::new(
            FakeSignal::new(Account {
                service_id: sid(ME),
                phone_number: Some("+15550000001".into()),
                kind: ProfileKind::Primary,
                device_name: "test".into(),
            })
            .with_sticker_pack(pack, vec![1])
            .with_groups(vec![Group {
                id: group,
                title: "band".into(),
                members: vec![sid(ME), sid(2)],
                admins: vec![],
            }]),
        );
        let (ctx, _) = ctx_with_roots(&[]);
        let ctx = ToolContext {
            signal: signal.clone(),
            ..ctx
        };
        let registry = Registry::standard();
        let send = |pack: StickerPackId| {
            registry.call(
                &ctx,
                "send_group_sticker",
                json!({"group_id": group.to_string(), "pack_id": pack.to_string(), "sticker_id": 1}),
            )
        };
        let missing = send(StickerPackId::new([0xcd; 16])).await;
        assert!(
            missing.is_err(),
            "sent from an uninstalled pack: {missing:?}"
        );
        send(pack).await.unwrap();
        assert_eq!(
            signal.sent(),
            [(
                Recipient::Group(group),
                Outgoing::Sticker {
                    pack,
                    sticker_id: 1
                }
            )]
        );
    }
}
