//! Imports the history archive a phone uploads for a newly linked device.
//! docs/link-and-sync.md traces the protocol; this module is the half after the
//! download: verify, decrypt, parse, and write messages to the store.

use std::{collections::HashMap, fmt};

use anyhow::Context;
use futures::AsyncReadExt as _;
use libsignal_account_keys::BackupKey;
use libsignal_core::Aci;
use libsignal_message_backup::{
    frame::{CursorFactory, FramesReader},
    key::MessageBackupKey,
    parse::VarintDelimitedReader,
    proto::backup as pb,
};
use protobuf::Message as _;
use relay_core::prelude::*;
use relay_store::MessageStore;
use zkgroup::groups::{GroupMasterKey, GroupSecretParams};

use crate::ImportProgress;

/// The 32-byte key the phone sends in the provisioning message when it offers
/// to transfer history. Single-use; never persist it.
pub struct LinkAndSyncKey([u8; 32]);

impl LinkAndSyncKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for LinkAndSyncKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkAndSyncKey(..)")
    }
}

/// What an import wrote and what it had to leave out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub imported: u64,
    pub skipped: u64,
}

/// Imports a downloaded link-and-sync archive into `store`.
///
/// `archive` is the file exactly as fetched from the CDN, `key` the provisioning
/// message's ephemeral backup key, and `our_aci` the linked account's ACI, which
/// the archive's encryption key is bound to. Fails without writing anything if
/// the archive doesn't authenticate under that key and ACI.
pub async fn import_link_and_sync(
    archive: &[u8],
    key: &LinkAndSyncKey,
    our_aci: ServiceId,
    store: &dyn MessageStore,
    progress: &mut (dyn FnMut(ImportProgress) + Send),
) -> anyhow::Result<ImportSummary> {
    let plaintext = decrypt(archive, key, our_aci)?;
    let mut reader = VarintDelimitedReader::new(futures::io::Cursor::new(plaintext.as_slice()));
    reader
        .read_next()
        .await
        .context("link-and-sync archive is not a valid backup stream")?
        .context("link-and-sync archive is empty")?;
    let mut importer = Importer {
        our_aci,
        store,
        recipients: HashMap::new(),
        titles: HashMap::new(),
        chats: HashMap::new(),
        read: Vec::new(),
        summary: ImportSummary::default(),
    };
    while let Some(raw) = reader
        .read_next()
        .await
        .context("link-and-sync archive is not a valid backup stream")?
    {
        let frame = pb::Frame::parse_from_bytes(&raw)
            .context("malformed frame in link-and-sync archive")?;
        let before = importer.summary.imported;
        importer.frame(frame).await?;
        if importer.summary.imported != before {
            progress(ImportProgress {
                imported: importer.summary.imported,
                total: None,
            });
        }
    }
    store
        .mark_read(&importer.read)
        .await
        .context("marking imported messages read")?;
    Ok(importer.summary)
}

/// Authenticates and decrypts the archive into its varint-delimited frames.
// NOTE: libsignal's FramesReader holds an Rc, so it can't live across an await
// in a future the agent spawns; decrypting up front keeps the import Send. It
// costs the gunzipped archive in memory (text only, so tens of MB for years of
// history) and blocks the calling thread for the decrypt, which is CPU and
// memory work with no I/O.
// FramesReader authenticates the whole archive before yielding any plaintext,
// then reads it a second time to decrypt. Both passes read one immutable
// buffer, so libsignal's end-of-stream HMAC re-check, which guards against a
// file changing between passes, has nothing to catch and isn't called.
fn decrypt(archive: &[u8], key: &LinkAndSyncKey, our_aci: ServiceId) -> anyhow::Result<Vec<u8>> {
    let backup_key: BackupKey = BackupKey(key.0);
    let backup_id = backup_key.derive_backup_id(&Aci::from(our_aci.uuid()));
    let message_key = MessageBackupKey::derive(&backup_key, &backup_id, None);
    futures::executor::block_on(async {
        let mut frames = FramesReader::new(&message_key, CursorFactory::new(&archive))
            .await
            .context(
                "link-and-sync archive failed to authenticate; wrong key or ACI, or a corrupt download",
            )?;
        let mut plaintext = Vec::new();
        frames
            .read_to_end(&mut plaintext)
            .await
            .context("link-and-sync archive failed to decrypt")?;
        Ok(plaintext)
    })
}

/// Who a backup recipient id stands for, as far as messages are concerned.
#[derive(Clone, Copy)]
enum Peer {
    Me,
    Contact(ServiceId),
    Group(GroupId),
}

/// Frames refer to recipients and chats by ids local to the archive, defined
/// before first use; these maps resolve them.
struct Importer<'a> {
    our_aci: ServiceId,
    store: &'a dyn MessageStore,
    recipients: HashMap<u64, Peer>,
    titles: HashMap<u64, String>,
    chats: HashMap<u64, Recipient>,
    read: Vec<MessageId>,
    summary: ImportSummary,
}

impl Importer<'_> {
    async fn frame(&mut self, frame: pb::Frame) -> anyhow::Result<()> {
        match frame.item {
            Some(pb::frame::Item::Recipient(r)) => self.recipient(r),
            Some(pb::frame::Item::Chat(c)) => self.chat(c).await?,
            Some(pb::frame::Item::ChatItem(item)) => self.chat_item(item).await?,
            _ => {}
        }
        Ok(())
    }

    fn recipient(&mut self, recipient: pb::Recipient) {
        let peer = match recipient.destination {
            Some(pb::recipient::Destination::Self_(_)) => Peer::Me,
            Some(pb::recipient::Destination::Contact(c)) => {
                let Some(aci) = c.aci.as_deref().and_then(service_id) else {
                    return;
                };
                let nickname = c
                    .nickname
                    .as_ref()
                    .map(|n| [n.given.as_str(), n.family.as_str()]);
                let system = [c.systemGivenName.as_str(), c.systemFamilyName.as_str()];
                let profile = [
                    c.profileGivenName.as_deref().unwrap_or_default(),
                    c.profileFamilyName.as_deref().unwrap_or_default(),
                ];
                if let Some(name) = [nickname, Some(system), Some(profile)]
                    .into_iter()
                    .flatten()
                    .find_map(full_name)
                {
                    self.titles.insert(recipient.id, name);
                }
                Peer::Contact(aci)
            }
            Some(pb::recipient::Destination::Group(g)) => {
                let Ok(master_key) = <[u8; 32]>::try_from(g.masterKey.as_slice()) else {
                    return;
                };
                if let Some(pb::group::group_attribute_blob::Content::Title(title)) =
                    g.snapshot.title.as_ref().and_then(|t| t.content.as_ref())
                    && !title.is_empty()
                {
                    self.titles.insert(recipient.id, title.clone());
                }
                Peer::Group(group_id(master_key))
            }
            _ => return,
        };
        self.recipients.insert(recipient.id, peer);
    }

    async fn chat(&mut self, chat: pb::Chat) -> anyhow::Result<()> {
        let conversation = match self.recipients.get(&chat.recipientId) {
            Some(Peer::Me) => Recipient::Contact(self.our_aci),
            Some(Peer::Contact(aci)) => Recipient::Contact(*aci),
            Some(Peer::Group(id)) => Recipient::Group(*id),
            None => return Ok(()),
        };
        self.chats.insert(chat.id, conversation);
        if let Some(title) = self.titles.get(&chat.recipientId) {
            self.store
                .set_title(conversation, title)
                .await
                .with_context(|| format!("titling imported conversation {conversation:?}"))?;
        }
        Ok(())
    }

    /// The ACI behind a recipient id, if it's a person we can attribute to.
    fn person(&self, recipient_id: u64) -> Option<ServiceId> {
        match self.recipients.get(&recipient_id)? {
            Peer::Me => Some(self.our_aci),
            Peer::Contact(aci) => Some(*aci),
            Peer::Group(_) => None,
        }
    }

    async fn chat_item(&mut self, item: pb::ChatItem) -> anyhow::Result<()> {
        if matches!(
            item.item,
            None | Some(pb::chat_item::Item::UpdateMessage(_))
        ) {
            self.summary.skipped += 1;
            return Ok(());
        }
        let author = self.person(item.authorId);
        let (Some(author), Some(&conversation)) = (author, self.chats.get(&item.chatId)) else {
            self.summary.skipped += 1;
            return Ok(());
        };
        let (_, read) = arrival(&item);
        let original = item.revisions.first().unwrap_or(&item);
        let (received_at_ms, _) = arrival(original);
        let standard = match &item.item {
            Some(pb::chat_item::Item::StandardMessage(m)) => Some(m),
            _ => None,
        };
        let body = standard
            .and_then(|m| m.text.as_ref())
            .map(|t| t.body.clone());
        let quote = standard.and_then(|m| m.quote.as_ref()).and_then(|q| {
            Some(MessageId {
                author: self.person(q.authorId)?,
                sent_at_ms: q.targetSentTimestamp?,
            })
        });
        let message = Message {
            id: MessageId {
                author,
                sent_at_ms: original.dateSent,
            },
            conversation,
            sender: author,
            from_me: author == self.our_aci,
            received_at_ms,
            body,
            attachments: vec![],
            quote,
            edited: !item.revisions.is_empty(),
            deleted: matches!(
                item.item,
                Some(pb::chat_item::Item::RemoteDeletedMessage(_))
            ),
        };
        self.store
            .insert(&message)
            .await
            .with_context(|| format!("storing imported message {:?}", message.id))?;
        if read {
            self.read.push(message.id);
        }
        self.summary.imported += 1;
        Ok(())
    }
}

/// Given and family name joined, or None if both are empty.
fn full_name(parts: [&str; 2]) -> Option<String> {
    let parts: Vec<_> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// When a chat item arrived and whether it had been read.
fn arrival(item: &pb::ChatItem) -> (u64, bool) {
    match &item.directionalDetails {
        Some(pb::chat_item::DirectionalDetails::Incoming(d)) => (d.dateReceived, d.read),
        _ => (item.dateSent, false),
    }
}

/// The group's public identifier. The master key is a secret, so it never
/// leaves this function.
fn group_id(master_key: [u8; 32]) -> GroupId {
    GroupId::new(
        GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key))
            .get_group_identifier(),
    )
}

fn service_id(bytes: &[u8]) -> Option<ServiceId> {
    let bytes: [u8; 16] = bytes.try_into().ok()?;
    Some(ServiceId::new(uuid::Uuid::from_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsignal_account_keys::AccountEntropyPool;
    use libsignal_message_backup::export;
    use relay_store::MemoryStore;

    // libsignal v0.99.0 rust/message-backup/tests/res/test-cases/valid-encrypted/,
    // with the account entropy pool and ACI its test_cases.rs encrypts it under.
    // It's in the legacy layout link-and-sync uses: no forward-secrecy header.
    const LIBSIGNAL_LEGACY_VECTOR: &[u8] =
        include_bytes!("testdata/legacy-account.binproto.encrypted");
    const LIBSIGNAL_VECTOR_POOL: &str =
        "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";
    const LIBSIGNAL_VECTOR_ACI: [u8; 16] = [0x11; 16];

    fn libsignal_vector_key() -> LinkAndSyncKey {
        let pool: AccountEntropyPool = LIBSIGNAL_VECTOR_POOL.parse().expect("valid pool");
        LinkAndSyncKey::new(BackupKey::derive_from_account_entropy_pool(&pool).0)
    }

    fn libsignal_vector_aci() -> ServiceId {
        ServiceId::new(uuid::Uuid::from_bytes(LIBSIGNAL_VECTOR_ACI))
    }

    /// The key from provisioning is a BackupKey used as-is with our ACI and no
    /// forward-secrecy token; libsignal's own legacy vector must decrypt that way.
    #[tokio::test]
    async fn libsignals_legacy_vector_decrypts_with_the_key_used_as_a_backup_key() {
        let store = MemoryStore::default();
        let summary = import_link_and_sync(
            LIBSIGNAL_LEGACY_VECTOR,
            &libsignal_vector_key(),
            libsignal_vector_aci(),
            &store,
            &mut |_| {},
        )
        .await
        .expect("libsignal's vector should import");
        assert_eq!(
            summary,
            ImportSummary::default(),
            "account-only vector has no messages"
        );
    }

    /// The archive key is bound to our ACI; an archive for another account must
    /// be refused, not decrypted into garbage.
    #[tokio::test]
    async fn an_archive_for_another_aci_is_refused() {
        let store = MemoryStore::default();
        let result = import_link_and_sync(
            LIBSIGNAL_LEGACY_VECTOR,
            &libsignal_vector_key(),
            ServiceId::new(uuid::Uuid::from_u128(1)),
            &store,
            &mut |_| {},
        )
        .await;
        assert!(result.is_err(), "imported under the wrong ACI: {result:?}");
    }

    const OUR_ACI: u128 = 0xAA;
    const ALICE_ACI: u128 = 0xA1;
    const SELF_ID: u64 = 1;
    const ALICE_ID: u64 = 2;
    const ALICE_CHAT: u64 = 10;

    fn aci(n: u128) -> ServiceId {
        ServiceId::new(uuid::Uuid::from_u128(n))
    }

    fn synthetic_key() -> LinkAndSyncKey {
        LinkAndSyncKey::new([7; 32])
    }

    /// Builds an archive the way the phone does: varint-delimited frames after a
    /// BackupInfo, gzipped, AES-256-CBC under the derived key, then HMAC'd.
    fn archive(frames: Vec<pb::Frame>) -> Vec<u8> {
        let mut plain = pb::BackupInfo {
            version: 1,
            backupTimeMs: 1,
            ..Default::default()
        }
        .write_length_delimited_to_bytes()
        .expect("encode info");
        for frame in frames {
            plain.extend(
                frame
                    .write_length_delimited_to_bytes()
                    .expect("encode frame"),
            );
        }
        let backup_key: BackupKey = BackupKey(synthetic_key().0);
        let id = backup_key.derive_backup_id(&Aci::from(aci(OUR_ACI).uuid()));
        let key = MessageBackupKey::derive(&backup_key, &id, None);
        let iv = [b'I'; 16];
        let mut body = export::gzip_compress(futures::io::Cursor::new(plain));
        export::aes_cbc_encrypt(&key.aes_key, &iv, &mut body);
        let hmac = export::hmac_checksum(&key.hmac_key, &iv, &body);
        [iv.as_slice(), &body, &hmac].concat()
    }

    fn self_recipient() -> pb::Frame {
        recipient(
            SELF_ID,
            pb::recipient::Destination::Self_(Default::default()),
        )
    }

    fn contact(id: u64, aci: u128) -> pb::Frame {
        recipient(
            id,
            pb::recipient::Destination::Contact(pb::Contact {
                aci: Some(uuid::Uuid::from_u128(aci).as_bytes().to_vec()),
                ..Default::default()
            }),
        )
    }

    fn recipient(id: u64, destination: pb::recipient::Destination) -> pb::Frame {
        pb::Frame {
            item: Some(pb::frame::Item::Recipient(pb::Recipient {
                id,
                destination: Some(destination),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn chat(id: u64, recipient_id: u64) -> pb::Frame {
        pb::Frame {
            item: Some(pb::frame::Item::Chat(pb::Chat {
                id,
                recipientId: recipient_id,
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn incoming_text(chat_id: u64, author_id: u64, sent: u64, body: &str) -> pb::ChatItem {
        pb::ChatItem {
            chatId: chat_id,
            authorId: author_id,
            dateSent: sent,
            directionalDetails: Some(pb::chat_item::DirectionalDetails::Incoming(
                pb::chat_item::IncomingMessageDetails {
                    dateReceived: sent + 5,
                    ..Default::default()
                },
            )),
            item: Some(pb::chat_item::Item::StandardMessage(pb::StandardMessage {
                text: Some(pb::Text {
                    body: body.to_owned(),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn item(chat_item: pb::ChatItem) -> pb::Frame {
        pb::Frame {
            item: Some(pb::frame::Item::ChatItem(chat_item)),
            ..Default::default()
        }
    }

    async fn import(frames: Vec<pb::Frame>) -> (MemoryStore, ImportSummary) {
        let store = MemoryStore::default();
        let summary = import_link_and_sync(
            &archive(frames),
            &synthetic_key(),
            aci(OUR_ACI),
            &store,
            &mut |_| {},
        )
        .await
        .expect("synthetic archive should import");
        (store, summary)
    }

    async fn all_messages(store: &MemoryStore) -> Vec<Message> {
        let mut out = vec![];
        for c in store.conversations().await.expect("conversations") {
            out.extend(
                store
                    .history(c.recipient, None, u32::MAX)
                    .await
                    .expect("history"),
            );
        }
        out
    }

    fn outgoing_text(chat_id: u64, sent: u64, body: &str) -> pb::ChatItem {
        pb::ChatItem {
            directionalDetails: Some(pb::chat_item::DirectionalDetails::Outgoing(
                Default::default(),
            )),
            ..incoming_text(chat_id, SELF_ID, sent, body)
        }
    }

    fn group(id: u64, master_key: [u8; 32]) -> pb::Frame {
        recipient(
            id,
            pb::recipient::Destination::Group(pb::Group {
                masterKey: master_key.to_vec(),
                ..Default::default()
            }),
        )
    }

    /// The point of link-and-sync: a text a contact sent before linking must
    /// land in the store under their conversation with its original timestamps.
    #[tokio::test]
    async fn an_incoming_text_lands_in_the_senders_conversation() {
        let (store, summary) = import(vec![
            self_recipient(),
            contact(ALICE_ID, ALICE_ACI),
            chat(ALICE_CHAT, ALICE_ID),
            item(incoming_text(
                ALICE_CHAT,
                ALICE_ID,
                1_000,
                "thirty days ago",
            )),
        ])
        .await;
        assert_eq!(summary.imported, 1, "{summary:?}");
        let stored = all_messages(&store).await;
        let expected = Message {
            id: MessageId {
                author: aci(ALICE_ACI),
                sent_at_ms: 1_000,
            },
            conversation: Recipient::Contact(aci(ALICE_ACI)),
            sender: aci(ALICE_ACI),
            from_me: false,
            received_at_ms: 1_005,
            body: Some("thirty days ago".to_owned()),
            attachments: vec![],
            quote: None,
            edited: false,
            deleted: false,
        };
        assert_eq!(stored, vec![expected]);
    }

    /// What I sent to Alice belongs in Alice's conversation, attributed to me.
    #[tokio::test]
    async fn an_outgoing_text_is_mine_in_the_contacts_conversation() {
        let (store, _) = import(vec![
            self_recipient(),
            contact(ALICE_ID, ALICE_ACI),
            chat(ALICE_CHAT, ALICE_ID),
            item(outgoing_text(ALICE_CHAT, 2_000, "on my way")),
        ])
        .await;
        let stored = all_messages(&store).await;
        let [m] = stored.as_slice() else {
            panic!("expected one message, got {stored:?}");
        };
        assert_eq!(m.conversation, Recipient::Contact(aci(ALICE_ACI)));
        assert_eq!((m.sender, m.from_me), (aci(OUR_ACI), true), "{m:?}");
        assert_eq!(m.id.author, aci(OUR_ACI));
    }

    /// A group is keyed by the identifier derived from its master key, as live
    /// messages are, and the master key itself (a secret) must never be the id.
    #[tokio::test]
    async fn group_messages_are_keyed_by_the_derived_group_identifier() {
        let master_key = [9; 32];
        let (store, _) = import(vec![
            self_recipient(),
            contact(ALICE_ID, ALICE_ACI),
            group(3, master_key),
            chat(11, 3),
            item(incoming_text(11, ALICE_ID, 3_000, "group hello")),
        ])
        .await;
        let expected = GroupId::new(
            GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key))
                .get_group_identifier(),
        );
        assert_ne!(
            expected,
            GroupId::new(master_key),
            "identifier must not be the key"
        );
        let stored = all_messages(&store).await;
        let conversations: Vec<_> = stored.iter().map(|m| m.conversation).collect();
        assert_eq!(conversations, vec![Recipient::Group(expected)]);
    }

    /// relay-core identifies people by ACI; a PNI-only contact's messages can't
    /// be attributed, so they're skipped and counted rather than misfiled.
    #[tokio::test]
    async fn messages_from_a_contact_without_an_aci_are_skipped_and_counted() {
        let pni_only = recipient(
            ALICE_ID,
            pb::recipient::Destination::Contact(pb::Contact {
                pni: Some(vec![5; 16]),
                ..Default::default()
            }),
        );
        let (store, summary) = import(vec![
            self_recipient(),
            pni_only,
            chat(ALICE_CHAT, ALICE_ID),
            item(incoming_text(ALICE_CHAT, ALICE_ID, 1_000, "who am i")),
        ])
        .await;
        assert_eq!(
            summary,
            ImportSummary {
                imported: 0,
                skipped: 1
            }
        );
        let stored = all_messages(&store).await;
        assert!(
            stored.is_empty(),
            "stored unattributable messages: {stored:?}"
        );
    }

    /// A fresh link must not report years of history the phone already read as
    /// unread; only what was unread on the phone stays unread.
    #[tokio::test]
    async fn messages_read_on_the_phone_import_as_read() {
        let mut read = incoming_text(ALICE_CHAT, ALICE_ID, 1_000, "seen");
        if let Some(pb::chat_item::DirectionalDetails::Incoming(d)) = &mut read.directionalDetails {
            d.read = true;
        }
        let (store, _) = import(vec![
            self_recipient(),
            contact(ALICE_ID, ALICE_ACI),
            chat(ALICE_CHAT, ALICE_ID),
            item(read),
            item(incoming_text(ALICE_CHAT, ALICE_ID, 2_000, "not yet seen")),
        ])
        .await;
        let unread = store.unread().await.expect("unread");
        let bodies: Vec<_> = unread.iter().filter_map(|m| m.body.as_deref()).collect();
        assert_eq!(bodies, vec!["not yet seen"]);
    }

    /// A message deleted for everyone before linking must import as deleted, so
    /// it stays out of search like a live delete does.
    #[tokio::test]
    async fn a_remotely_deleted_message_imports_as_deleted() {
        let deleted = pb::ChatItem {
            item: Some(pb::chat_item::Item::RemoteDeletedMessage(Default::default())),
            ..incoming_text(ALICE_CHAT, ALICE_ID, 1_000, "")
        };
        let (store, _) = import(vec![
            self_recipient(),
            contact(ALICE_ID, ALICE_ACI),
            chat(ALICE_CHAT, ALICE_ID),
            item(deleted),
        ])
        .await;
        let stored = all_messages(&store).await;
        let [m] = stored.as_slice() else {
            panic!("expected one message, got {stored:?}");
        };
        assert!(m.deleted && m.body.is_none(), "{m:?}");
    }

    /// An edited message carries its earlier versions as revisions and the
    /// edit's own timestamp on top. It imports once, with the latest text, under
    /// the original's timestamp, since that's what reactions and quotes target
    /// (Signal-Desktop's importer does the same).
    #[tokio::test]
    async fn an_edited_message_imports_once_with_its_latest_text() {
        let edited = pb::ChatItem {
            revisions: vec![incoming_text(ALICE_CHAT, ALICE_ID, 1_000, "frist")],
            ..incoming_text(ALICE_CHAT, ALICE_ID, 1_500, "first")
        };
        let (store, summary) = import(vec![
            self_recipient(),
            contact(ALICE_ID, ALICE_ACI),
            chat(ALICE_CHAT, ALICE_ID),
            item(edited),
        ])
        .await;
        assert_eq!(summary.imported, 1, "{summary:?}");
        let stored = all_messages(&store).await;
        let [m] = stored.as_slice() else {
            panic!("expected one message, got {stored:?}");
        };
        assert_eq!(
            (m.body.as_deref(), m.edited),
            (Some("first"), true),
            "{m:?}"
        );
    }

    /// A reply must still point at the message it quoted, by that message's
    /// author and send time.
    #[tokio::test]
    async fn a_reply_keeps_a_reference_to_the_quoted_message() {
        let mut reply = outgoing_text(ALICE_CHAT, 2_000, "agreed");
        if let Some(pb::chat_item::Item::StandardMessage(m)) = &mut reply.item {
            m.quote = Some(pb::Quote {
                targetSentTimestamp: Some(1_000),
                authorId: ALICE_ID,
                ..Default::default()
            })
            .into();
        }
        let (store, _) = import(vec![
            self_recipient(),
            contact(ALICE_ID, ALICE_ACI),
            chat(ALICE_CHAT, ALICE_ID),
            item(incoming_text(ALICE_CHAT, ALICE_ID, 1_000, "pizza?")),
            item(reply),
        ])
        .await;
        let stored = all_messages(&store).await;
        let quotes: Vec<_> = stored.iter().filter_map(|m| m.quote).collect();
        assert_eq!(
            quotes,
            vec![MessageId {
                author: aci(ALICE_ACI),
                sent_at_ms: 1_000
            }]
        );
    }

    /// Chat events (someone joined, the timer changed) aren't messages; storing
    /// them would fill history and unread counts with empty rows.
    #[tokio::test]
    async fn chat_update_events_are_not_stored_as_messages() {
        let update = pb::ChatItem {
            directionalDetails: Some(pb::chat_item::DirectionalDetails::Directionless(
                Default::default(),
            )),
            item: Some(pb::chat_item::Item::UpdateMessage(Default::default())),
            ..incoming_text(ALICE_CHAT, ALICE_ID, 1_000, "")
        };
        let (store, summary) = import(vec![
            self_recipient(),
            contact(ALICE_ID, ALICE_ACI),
            chat(ALICE_CHAT, ALICE_ID),
            item(update),
        ])
        .await;
        let stored = all_messages(&store).await;
        assert!(stored.is_empty(), "stored a chat event: {stored:?}");
        assert_eq!(
            summary,
            ImportSummary {
                imported: 0,
                skipped: 1
            }
        );
    }

    /// Setup shows import progress; it must advance with every message and end
    /// at the summary's count.
    #[tokio::test]
    async fn progress_counts_every_imported_message() {
        let store = MemoryStore::default();
        let mut seen = vec![];
        let summary = import_link_and_sync(
            &archive(vec![
                self_recipient(),
                contact(ALICE_ID, ALICE_ACI),
                chat(ALICE_CHAT, ALICE_ID),
                item(incoming_text(ALICE_CHAT, ALICE_ID, 1_000, "one")),
                item(incoming_text(ALICE_CHAT, ALICE_ID, 2_000, "two")),
            ]),
            &synthetic_key(),
            aci(OUR_ACI),
            &store,
            &mut |p| seen.push(p.imported),
        )
        .await
        .expect("import");
        assert_eq!(seen, vec![1, 2], "progress: {seen:?}");
        assert_eq!(seen.last().copied(), Some(summary.imported));
    }

    /// Conversation lists need names; imported chats take the contact's profile
    /// name and the group's title from the archive.
    #[tokio::test]
    async fn conversations_are_titled_from_profile_names_and_group_titles() {
        let alice = recipient(
            ALICE_ID,
            pb::recipient::Destination::Contact(pb::Contact {
                aci: Some(uuid::Uuid::from_u128(ALICE_ACI).as_bytes().to_vec()),
                profileGivenName: Some("Alice".to_owned()),
                profileFamilyName: Some("Liddell".to_owned()),
                ..Default::default()
            }),
        );
        let mut book_club = pb::Group {
            masterKey: vec![9; 32],
            ..Default::default()
        };
        book_club.snapshot.mut_or_insert_default().title = Some(pb::group::GroupAttributeBlob {
            content: Some(pb::group::group_attribute_blob::Content::Title(
                "Book club".to_owned(),
            )),
            ..Default::default()
        })
        .into();
        let (store, _) = import(vec![
            self_recipient(),
            alice,
            recipient(3, pb::recipient::Destination::Group(book_club)),
            chat(ALICE_CHAT, ALICE_ID),
            chat(11, 3),
            item(incoming_text(ALICE_CHAT, ALICE_ID, 1_000, "hi")),
            item(incoming_text(11, ALICE_ID, 2_000, "chapter 3?")),
        ])
        .await;
        let mut titles: Vec<_> = store
            .conversations()
            .await
            .expect("conversations")
            .into_iter()
            .map(|c| c.title)
            .collect();
        titles.sort();
        assert_eq!(titles, vec!["Alice Liddell", "Book club"]);
    }

    /// The name the user gave a contact (a nickname, or their address book
    /// entry) beats the name the contact chose, as in Signal's own apps, so
    /// "message Mom" finds the right chat.
    #[tokio::test]
    async fn names_the_user_chose_beat_profile_names() {
        let named = |id: u64, aci: u128, nickname: &str, system: &str| {
            recipient(
                id,
                pb::recipient::Destination::Contact(pb::Contact {
                    aci: Some(uuid::Uuid::from_u128(aci).as_bytes().to_vec()),
                    profileGivenName: Some("Profile".to_owned()),
                    systemGivenName: system.to_owned(),
                    nickname: (!nickname.is_empty())
                        .then(|| pb::contact::Name {
                            given: nickname.to_owned(),
                            ..Default::default()
                        })
                        .into(),
                    ..Default::default()
                }),
            )
        };
        let (store, _) = import(vec![
            self_recipient(),
            named(2, 0xB1, "Mom", "Jane"),
            named(3, 0xB2, "", "Dad"),
            chat(12, 2),
            chat(13, 3),
            item(incoming_text(12, 2, 1_000, "call me")),
            item(incoming_text(13, 3, 2_000, "call your mother")),
        ])
        .await;
        let mut titles: Vec<_> = store
            .conversations()
            .await
            .expect("conversations")
            .into_iter()
            .map(|c| c.title)
            .collect();
        titles.sort();
        assert_eq!(titles, vec!["Dad", "Mom"]);
    }

    /// The agent runs the import on a multithreaded runtime alongside the
    /// receive loop, so its future must be Send.
    #[test]
    fn the_import_future_is_send() {
        fn assert_send<T: Send>(_: T) {}
        let store = MemoryStore::default();
        let key = synthetic_key();
        let mut progress = |_| {};
        assert_send(import_link_and_sync(
            &[],
            &key,
            aci(OUR_ACI),
            &store,
            &mut progress,
        ));
    }
}
