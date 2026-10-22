//! Tools that send files and stickers. Files leave the machine only through
//! `SendRoots::admit`, so a prompt-injected client can't mail out a file the
//! user never put in a send folder.

use std::path::Path;

use anyhow::bail;
use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::SendRoots;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{Timestamp, contact, contact_schema, non_blank, parse},
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
                "caption": {"type": "string", "minLength": 1, "description": "Text shown with the files"},
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

fn path_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "description": "A file to send: an absolute path inside a send folder",
    })
}

fn paths_schema() -> Value {
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
fn files(path: Option<String>, paths: Option<Vec<String>>) -> Result<Vec<String>, ToolError> {
    if paths.as_ref().is_some_and(Vec::is_empty) {
        return Err(ToolError::InvalidInput(
            "paths must name at least one file".into(),
        ));
    }
    let all: Vec<_> = path
        .into_iter()
        .chain(paths.into_iter().flatten())
        .collect();
    if !(1..=MAX_FILES).contains(&all.len()) {
        return Err(ToolError::InvalidInput(format!(
            "send 1 to {MAX_FILES} files, given as path or paths; got {}",
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

    use relay_signal::{Account, FakeSignal, Outgoing, SendRoots, SignalBackend};
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
        let result = Registry::standard()
            .call(ctx, "send_attachment", input.clone())
            .await;
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
}
