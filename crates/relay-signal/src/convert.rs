//! The only place presage and libsignal types meet relay-core types, so the
//! protocol's shape changes in one file when presage moves.

use std::path::Path;

use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use presage::libsignal_service::prelude::{
    ProtobufMessage,
    phonenumber::{self, PhoneNumber},
};
use presage::{
    libsignal_service::{
        Profile,
        content::{Content, ContentBody},
        groups_v2::Role,
        profile_name::ProfileName,
        protocol,
        sender::AttachmentSpec,
        websocket::account::DeviceInfo,
        zkgroup::groups::{GroupMasterKey, GroupSecretParams},
    },
    manager::{RegistrationData, RegistrationType},
    proto::{
        AttachmentPointer, DataMessage, EditMessage, GroupContextV2, ReceiptMessage, SyncMessage,
        TypingMessage, data_message, receipt_message,
        sync_message::{self, Content as SyncContent},
        typing_message,
    },
    store::Thread,
};
use relay_core::prelude::*;

use crate::{
    Account, Incoming, Outgoing, ProfileUpdate, RequestResponse, StickerInfo, StickerPack,
};

/// What `content` means to the agent, or `None` when it carries nothing the
/// agent keeps (delivery receipts, calls, stories, key syncs). `me` is the
/// account's own ACI.
pub(crate) fn incoming(content: &Content, me: ServiceId) -> Option<Incoming> {
    let sender = service_id(content.metadata.sender);
    let conversation = conversation(content)?;
    let data = match &content.body {
        ContentBody::DataMessage(data) => data,
        ContentBody::SynchronizeMessage(SyncMessage {
            content:
                Some(SyncContent::Sent(sync_message::Sent {
                    message: Some(data),
                    ..
                })),
            ..
        }) => data,
        ContentBody::EditMessage(edit)
        | ContentBody::SynchronizeMessage(SyncMessage {
            content:
                Some(SyncContent::Sent(sync_message::Sent {
                    edit_message: Some(edit),
                    ..
                })),
            ..
        }) => {
            return Some(Incoming::Edit {
                conversation,
                target: MessageId {
                    author: sender,
                    sent_at_ms: edit.target_sent_timestamp?,
                },
                from: sender,
                body: edit.data_message.as_ref()?.body.clone()?,
            });
        }
        ContentBody::ReceiptMessage(receipt) if receipt.r#type() == receipt_message::Type::Read => {
            let targets = receipt.timestamp.iter().map(|&sent_at_ms| MessageId {
                author: me,
                sent_at_ms,
            });
            return Some(Incoming::Read {
                targets: targets.collect(),
            });
        }
        ContentBody::SynchronizeMessage(sync) if !sync.read.is_empty() => {
            let targets = sync.read.iter().filter_map(|read| {
                Some(MessageId {
                    author: aci_of(
                        read.sender_aci_binary.as_deref(),
                        read.sender_aci.as_deref(),
                    )?,
                    sent_at_ms: read.timestamp?,
                })
            });
            return Some(Incoming::Read {
                targets: targets.collect(),
            });
        }
        _ => return None,
    };
    if let Some(reaction) = &data.reaction {
        return Some(Incoming::Reaction {
            conversation,
            target: MessageId {
                author: aci_of(
                    reaction.target_author_aci_binary.as_deref(),
                    reaction.target_author_aci.as_deref(),
                )?,
                sent_at_ms: reaction.target_sent_timestamp?,
            },
            from: sender,
            emoji: reaction.emoji.clone()?,
            remove: reaction.remove.unwrap_or(false),
        });
    }
    if let Some(delete) = &data.delete {
        return Some(Incoming::Delete {
            conversation,
            target: MessageId {
                author: sender,
                sent_at_ms: delete.target_sent_timestamp?,
            },
            from: sender,
        });
    }
    if let Some(delete) = &data.admin_delete {
        let Recipient::Group(group) = conversation else {
            return None;
        };
        return Some(Incoming::AdminDelete {
            group,
            target: MessageId {
                author: aci_of(delete.target_author_aci_binary.as_deref(), None)?,
                sent_at_ms: delete.target_sent_timestamp?,
            },
            from: sender,
        });
    }
    Some(Incoming::Message(Message {
        id: MessageId {
            author: sender,
            sent_at_ms: data.timestamp?,
        },
        conversation,
        sender,
        from_me: sender == me,
        received_at_ms: millis(content),
        body: data.body.clone(),
        attachments: data.attachments.iter().map(attachment_ref).collect(),
        quote: data.quote.as_ref().and_then(|quote| {
            Some(MessageId {
                author: aci_of(
                    quote.author_aci_binary.as_deref(),
                    quote.author_aci.as_deref(),
                )?,
                sent_at_ms: quote.id?,
            })
        }),
        edited: false,
        deleted: false,
    }))
}

/// An attachment's metadata, with an id that carries everything needed to
/// download and decrypt it later.
// NOTE: the id embeds the attachment's decryption key. That's what lets the
// agent fetch bytes after a restart without presage's message store, and it
// grants nothing the message text doesn't already, but it must never be logged
// as if it were an opaque handle.
pub(crate) fn attachment_ref(pointer: &AttachmentPointer) -> AttachmentRef {
    AttachmentRef {
        id: URL_SAFE_NO_PAD.encode(pointer.encode_to_vec()),
        content_type: pointer.content_type.clone().unwrap_or_default(),
        file_name: pointer.file_name.clone(),
        size: pointer.size.map(u64::from).unwrap_or_default(),
    }
}

/// The pointer an `attachment_ref` id was made from.
pub(crate) fn attachment_pointer(attachment: &AttachmentRef) -> anyhow::Result<AttachmentPointer> {
    let bytes = URL_SAFE_NO_PAD
        .decode(&attachment.id)
        .context("attachment id is not one this backend issued")?;
    AttachmentPointer::decode(bytes.as_slice())
        .context("attachment id is not one this backend issued")
}

/// How to upload the file at `path`, `length` bytes long.
pub(crate) fn attachment_spec(path: &Path, length: usize) -> AttachmentSpec {
    AttachmentSpec {
        content_type: mime_guess::from_path(path)
            .first_or_octet_stream()
            .essence_str()
            .to_owned(),
        length,
        file_name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        ..Default::default()
    }
}

pub(crate) fn sticker_pack_from(pack: &presage::store::StickerPack) -> anyhow::Result<StickerPack> {
    let id: [u8; 16] =
        pack.id.as_slice().try_into().with_context(|| {
            format!("stored sticker pack id is {} bytes, not 16", pack.id.len())
        })?;
    Ok(StickerPack {
        id: StickerPackId::new(id),
        title: pack.manifest.title.clone(),
        author: pack.manifest.author.clone(),
        stickers: pack
            .manifest
            .stickers
            .iter()
            .map(|s| StickerInfo {
                id: s.id,
                emoji: s.emoji.clone(),
            })
            .collect(),
    })
}

/// What to put on the wire for `message`, sent at `sent_at_ms`, carrying
/// `group` when it goes to a group.
pub(crate) fn content_body(
    message: &Outgoing,
    sent_at_ms: u64,
    group: Option<&GroupContextV2>,
) -> anyhow::Result<ContentBody> {
    let data = |message: DataMessage| DataMessage {
        timestamp: Some(sent_at_ms),
        group_v2: group.cloned(),
        ..message
    };
    Ok(match message {
        Outgoing::Text {
            body,
            quote,
            attachments,
        } => ContentBody::DataMessage(data(DataMessage {
            body: Some(body.clone()),
            attachments: attachments
                .iter()
                .map(attachment_pointer)
                .collect::<anyhow::Result<_>>()?,
            quote: quote.map(|quote| data_message::Quote {
                id: Some(quote.sent_at_ms),
                author_aci: Some(aci(quote.author).service_id_string()),
                author_aci_binary: Some(aci(quote.author).service_id_binary()),
                ..Default::default()
            }),
            ..Default::default()
        })),
        Outgoing::Reaction {
            target,
            emoji,
            remove,
        } => ContentBody::DataMessage(data(DataMessage {
            reaction: Some(data_message::Reaction {
                emoji: Some(emoji.clone()),
                remove: Some(*remove),
                target_author_aci: Some(aci(target.author).service_id_string()),
                target_author_aci_binary: Some(aci(target.author).service_id_binary()),
                target_sent_timestamp: Some(target.sent_at_ms),
            }),
            ..Default::default()
        })),
        Outgoing::Edit { target, body } => ContentBody::EditMessage(EditMessage {
            target_sent_timestamp: Some(target.sent_at_ms),
            data_message: Some(data(DataMessage {
                body: Some(body.clone()),
                ..Default::default()
            })),
        }),
        Outgoing::Delete { target } => ContentBody::DataMessage(data(DataMessage {
            delete: Some(data_message::Delete {
                target_sent_timestamp: Some(target.sent_at_ms),
            }),
            ..Default::default()
        })),
        Outgoing::Typing { started } => ContentBody::TypingMessage(TypingMessage {
            timestamp: Some(sent_at_ms),
            action: Some(
                if *started {
                    typing_message::Action::Started
                } else {
                    typing_message::Action::Stopped
                }
                .into(),
            ),
            group_id: group
                .and_then(|g| g.master_key.as_deref())
                .and_then(|key| <[u8; 32]>::try_from(key).ok())
                .map(|key| group_id(key).as_bytes().to_vec()),
        }),
        Outgoing::ReadReceipt { targets } => ContentBody::ReceiptMessage(ReceiptMessage {
            r#type: Some(receipt_message::Type::Read.into()),
            timestamp: targets.iter().map(|t| t.sent_at_ms).collect(),
        }),
        Outgoing::AdminDelete { .. } if group.is_none() => {
            bail!("an admin delete only goes to a group")
        }
        Outgoing::AdminDelete { target } => ContentBody::DataMessage(data(DataMessage {
            admin_delete: Some(data_message::AdminDelete {
                target_author_aci_binary: Some(aci(target.author).service_id_binary()),
                target_sent_timestamp: Some(target.sent_at_ms),
            }),
            ..Default::default()
        })),
        Outgoing::Pin {
            target,
            duration_secs,
        } => ContentBody::DataMessage(data(DataMessage {
            pin_message: Some(data_message::PinMessage {
                target_author_aci_binary: Some(aci(target.author).service_id_binary()),
                target_sent_timestamp: Some(target.sent_at_ms),
                pin_duration: Some(match duration_secs {
                    Some(secs) => {
                        data_message::pin_message::PinDuration::PinDurationSeconds(secs.get())
                    }
                    None => data_message::pin_message::PinDuration::PinDurationForever(true),
                }),
            }),
            ..Default::default()
        })),
        Outgoing::Unpin { target } => ContentBody::DataMessage(data(DataMessage {
            unpin_message: Some(data_message::UnpinMessage {
                target_author_aci_binary: Some(aci(target.author).service_id_binary()),
                target_sent_timestamp: Some(target.sent_at_ms),
            }),
            ..Default::default()
        })),
        Outgoing::PollCreate {
            question,
            options,
            multi_select,
        } => ContentBody::DataMessage(data(DataMessage {
            poll_create: Some(data_message::PollCreate {
                question: Some(question.clone()),
                allow_multiple: Some(*multi_select),
                options: options.clone(),
            }),
            ..Default::default()
        })),
        Outgoing::PollVote {
            poll,
            option_indexes,
            vote_count,
        } => ContentBody::DataMessage(data(DataMessage {
            poll_vote: Some(data_message::PollVote {
                target_author_aci_binary: Some(aci(poll.author).service_id_binary()),
                target_sent_timestamp: Some(poll.sent_at_ms),
                option_indexes: option_indexes.clone(),
                vote_count: Some(*vote_count),
            }),
            ..Default::default()
        })),
        Outgoing::PollTerminate { poll } => ContentBody::DataMessage(data(DataMessage {
            poll_terminate: Some(data_message::PollTerminate {
                target_sent_timestamp: Some(poll.sent_at_ms),
            }),
            ..Default::default()
        })),
        Outgoing::Sticker { .. } => {
            bail!("a sticker goes out through sticker_body, once its pack is looked up")
        }
        Outgoing::MessageRequestResponse { .. } => {
            bail!(
                "a message request answer goes out through profile_key_update and request_response_sync"
            )
        }
        Outgoing::ExpirationTimer { .. } if group.is_some() => {
            bail!("a timer update only goes to a contact; a group's timer is a group change")
        }
        Outgoing::ExpirationTimer { seconds } => ContentBody::DataMessage(data(DataMessage {
            expire_timer: Some(*seconds),
            flags: Some(data_message::Flags::ExpirationTimerUpdate as u32),
            ..Default::default()
        })),
    })
}

/// The name, about text and about emoji to upload: `update` laid over
/// `current`, since Signal replaces the whole profile on upload.
pub(crate) fn merged_profile(
    current: Profile,
    update: ProfileUpdate,
) -> anyhow::Result<(ProfileName<String>, Option<String>, Option<String>)> {
    let (given, family) = match current.name {
        Some(name) => (Some(name.given_name), name.family_name),
        None => (None, None),
    };
    let Some(given_name) = update.given_name.or(given).filter(|g| !g.trim().is_empty()) else {
        bail!("a Signal profile needs a given name");
    };
    let set = |new: Option<String>, old: Option<String>| match new {
        Some(new) if new.is_empty() => None,
        Some(new) => Some(new),
        None => old,
    };
    Ok((
        ProfileName {
            given_name,
            family_name: set(update.family_name, family),
        },
        set(update.about, current.about),
        set(update.about_emoji, current.about_emoji),
    ))
}

/// A message that only hands the recipient this account's profile key, which
/// presage attaches to every data message it sends.
pub(crate) fn profile_key_update(sent_at_ms: u64) -> ContentBody {
    ContentBody::DataMessage(DataMessage {
        timestamp: Some(sent_at_ms),
        flags: Some(data_message::Flags::ProfileKeyUpdate as u32),
        ..Default::default()
    })
}

/// Tells this account's other devices how it answered `thread`'s message
/// request.
pub(crate) fn request_response_sync(
    thread: ServiceId,
    response: RequestResponse,
) -> anyhow::Result<ContentBody> {
    use sync_message::message_request_response::Type;
    let r#type = match response {
        RequestResponse::Accept => Type::Accept,
        RequestResponse::Delete => Type::Delete,
    };
    Ok(ContentBody::SynchronizeMessage(SyncMessage {
        content: Some(SyncContent::MessageRequestResponse(
            sync_message::MessageRequestResponse {
                thread_aci_binary: Some(aci(thread).service_id_binary()),
                r#type: Some(r#type as i32),
                ..Default::default()
            },
        )),
        padding: Some(sync_padding()?),
        ..Default::default()
    }))
}

/// 1 to 512 random bytes, as Signal's apps pad sync messages so their size
/// doesn't give away what they carry.
fn sync_padding() -> anyhow::Result<Vec<u8>> {
    let mut size = [0u8; 2];
    getrandom::fill(&mut size).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    let mut padding = vec![0; 1 + usize::from(u16::from_le_bytes(size) % 512)];
    getrandom::fill(&mut padding).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    Ok(padding)
}

/// What to put on the wire for sticker `sticker_id` of `pack`, whose image
/// was uploaded as `image`.
pub(crate) fn sticker_body(
    pack: StickerPackId,
    pack_key: &[u8],
    sticker_id: u32,
    emoji: Option<&str>,
    image: AttachmentPointer,
    sent_at_ms: u64,
    group: Option<&GroupContextV2>,
) -> ContentBody {
    ContentBody::DataMessage(DataMessage {
        timestamp: Some(sent_at_ms),
        group_v2: group.cloned(),
        sticker: Some(data_message::Sticker {
            pack_id: Some(pack.as_bytes().to_vec()),
            pack_key: Some(pack_key.to_vec()),
            sticker_id: Some(sticker_id),
            data: Some(image),
            emoji: emoji.map(str::to_owned),
        }),
        ..Default::default()
    })
}

pub(crate) fn device(info: &DeviceInfo) -> crate::Device {
    crate::Device {
        id: u32::from(info.id),
        name: info.name.clone(),
        created_at_ms: u64::try_from(info.created_at.timestamp_millis()).unwrap_or_default(),
        last_seen_at_ms: u64::try_from(info.last_seen.timestamp_millis()).unwrap_or_default(),
    }
}

/// The safety number Signal's apps show for `me` and `them` with these keys.
pub(crate) fn safety_number(
    me: ServiceId,
    my_key: &protocol::IdentityKey,
    them: ServiceId,
    their_key: &protocol::IdentityKey,
) -> anyhow::Result<String> {
    // Version 2 and 5200 iterations, keyed by ACI bytes, as Signal's apps do.
    protocol::Fingerprint::new(
        2,
        5200,
        me.uuid().as_bytes(),
        my_key,
        them.uuid().as_bytes(),
        their_key,
    )
    .and_then(|fingerprint| fingerprint.display_string())
    .map_err(|e| anyhow::anyhow!("unable to compute the safety number with {them}: {e}"))
}

pub(crate) fn contact_from(contact: &presage::model::contacts::Contact) -> crate::Contact {
    crate::Contact {
        service_id: ServiceId::new(contact.uuid),
        name: Some(contact.name.clone()).filter(|name| !name.is_empty()),
        phone_number: contact.phone_number.as_ref().map(e164),
    }
}

pub(crate) fn group_from(
    master_key: [u8; 32],
    group: &presage::model::groups::Group,
) -> crate::Group {
    crate::Group {
        id: group_id(master_key),
        title: group.title.clone(),
        members: group
            .members
            .iter()
            .map(|member| ServiceId::new(member.aci.into()))
            .collect(),
        admins: group
            .members
            .iter()
            .filter(|member| member.role == Role::Administrator)
            .map(|member| ServiceId::new(member.aci.into()))
            .collect(),
    }
}

/// The account a registration describes.
pub(crate) fn account(data: &RegistrationData, kind: RegistrationType) -> Account {
    Account {
        service_id: ServiceId::new(data.service_ids.aci),
        phone_number: Some(e164(&data.phone_number)),
        kind: match kind {
            RegistrationType::Primary => ProfileKind::Primary,
            RegistrationType::Secondary => ProfileKind::Linked,
        },
        // A primary device has no name of its own on Signal.
        device_name: data.device_name().unwrap_or_default().to_owned(),
    }
}

pub(crate) fn e164(number: &PhoneNumber) -> String {
    number.format().mode(phonenumber::Mode::E164).to_string()
}

pub(crate) fn aci(id: ServiceId) -> protocol::Aci {
    protocol::Aci::from_uuid_bytes(id.uuid().into_bytes())
}

fn conversation(content: &Content) -> Option<Recipient> {
    Some(match Thread::try_from(content).ok()? {
        Thread::Contact(id) => Recipient::Contact(service_id(id)),
        Thread::Group(master_key) => Recipient::Group(group_id(master_key)),
    })
}

/// The group's public identifier. The master key is the group's secret and
/// never leaves this crate.
pub(crate) fn group_id(master_key: [u8; 32]) -> GroupId {
    let params = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key));
    GroupId::new(params.get_group_identifier())
}

/// An ACI from a proto's binary field, falling back to its string twin; newer
/// clients fill only the binary one, older ones only the string.
fn aci_of(binary: Option<&[u8]>, string: Option<&str>) -> Option<ServiceId> {
    let aci = match (binary, string) {
        (Some(bytes), _) => protocol::Aci::parse_from_service_id_binary(bytes)?,
        (None, Some(s)) => protocol::Aci::parse_from_service_id_string(s)?,
        (None, None) => return None,
    };
    Some(ServiceId::new(aci.into()))
}

fn service_id(id: protocol::ServiceId) -> ServiceId {
    ServiceId::new(id.raw_uuid())
}

fn millis(content: &Content) -> u64 {
    u64::try_from(content.metadata.server_timestamp.timestamp_millis()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use presage::libsignal_service::{
        content::Metadata,
        prelude::Uuid,
        proto::attachment_pointer,
        protocol::{Aci, DeviceId},
    };
    use proptest::prelude::*;

    const ALICE: Uuid = Uuid::from_u128(0xa11ce);
    const ME: Uuid = Uuid::from_u128(0x3e);

    fn aci(uuid: Uuid) -> Aci {
        Aci::from_uuid_bytes(uuid.into_bytes())
    }

    fn content_from(sender: Uuid, body: ContentBody) -> Content {
        let at = Utc
            .timestamp_millis_opt(1_700_000_000_500)
            .single()
            .expect("valid time");
        Content {
            metadata: Metadata {
                sender: aci(sender).into(),
                destination: aci(ME).into(),
                sender_device: DeviceId::new(1).expect("device 1"),
                pni_verified: None,
                client_timestamp: at,
                server_timestamp: at,
                needs_receipt: false,
                unidentified_sender: true,
                was_plaintext: false,
                server_guid: None,
            },
            body,
        }
    }

    /// A direct message is identified by its sender and the sender's timestamp,
    /// in the sender's conversation, or replies and reactions would point nowhere.
    #[test]
    fn direct_message_is_keyed_by_sender_and_sent_timestamp() {
        let content = content_from(
            ALICE,
            ContentBody::DataMessage(DataMessage {
                body: Some("hi".into()),
                timestamp: Some(1_700_000_000_000),
                ..Default::default()
            }),
        );
        let Some(Incoming::Message(message)) = incoming(&content, ServiceId::new(ME)) else {
            panic!(
                "not a message: {:?}",
                incoming(&content, ServiceId::new(ME))
            );
        };
        let alice = ServiceId::new(ALICE);
        assert_eq!(
            message.id,
            MessageId {
                author: alice,
                sent_at_ms: 1_700_000_000_000
            }
        );
        assert_eq!(message.conversation, Recipient::Contact(alice));
        assert_eq!(message.sender, alice);
        assert_eq!(message.body.as_deref(), Some("hi"));
        assert!(!message.from_me, "someone else's message marked from_me");
    }

    /// A group message belongs to the group's public id. Keying it by the master
    /// key would hand the group's secret to every MCP client that lists chats.
    #[test]
    fn group_message_is_keyed_by_public_group_id_not_master_key() {
        let master_key = [9u8; 32];
        let content = content_from(
            ALICE,
            ContentBody::DataMessage(DataMessage {
                body: Some("hi all".into()),
                timestamp: Some(5),
                group_v2: Some(GroupContextV2 {
                    master_key: Some(master_key.to_vec()),
                    revision: Some(1),
                    group_change: None,
                }),
                ..Default::default()
            }),
        );
        let Some(Incoming::Message(message)) = incoming(&content, ServiceId::new(ME)) else {
            panic!("not a message");
        };
        assert_eq!(message.conversation, Recipient::Group(group_id(master_key)));
        assert_ne!(
            message.conversation,
            Recipient::Group(GroupId::new(master_key)),
            "group keyed by its secret"
        );
    }

    /// A message sent from the account's phone arrives as a sync; it must land in
    /// the conversation it was sent to, marked as ours, or the history would show
    /// half of every conversation.
    #[test]
    fn message_synced_from_own_phone_is_from_me_in_the_destination_chat() {
        let content = content_from(
            ME,
            ContentBody::SynchronizeMessage(SyncMessage {
                content: Some(SyncContent::Sent(sync_message::Sent {
                    destination_service_id: Some(aci(ALICE).service_id_string()),
                    timestamp: Some(77),
                    message: Some(DataMessage {
                        body: Some("on my way".into()),
                        timestamp: Some(77),
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
                ..Default::default()
            }),
        );
        let Some(Incoming::Message(message)) = incoming(&content, ServiceId::new(ME)) else {
            panic!(
                "sync not mapped: {:?}",
                incoming(&content, ServiceId::new(ME))
            );
        };
        assert!(message.from_me, "own message not marked from_me");
        assert_eq!(
            message.conversation,
            Recipient::Contact(ServiceId::new(ALICE))
        );
        assert_eq!(message.body.as_deref(), Some("on my way"));
        assert_eq!(message.id.author, ServiceId::new(ME));
    }

    /// Newer Signal clients send the target author only in binary form; a
    /// reaction from one must still point at the right message.
    #[test]
    fn reaction_with_binary_only_author_targets_that_author() {
        let content = content_from(
            ALICE,
            ContentBody::DataMessage(DataMessage {
                timestamp: Some(90),
                reaction: Some(data_message::Reaction {
                    emoji: Some("🐈".into()),
                    remove: Some(false),
                    target_author_aci: None,
                    target_author_aci_binary: Some(ME.as_bytes().to_vec()),
                    target_sent_timestamp: Some(80),
                }),
                ..Default::default()
            }),
        );
        let event = incoming(&content, ServiceId::new(ME));
        assert_eq!(
            event,
            Some(Incoming::Reaction {
                conversation: Recipient::Contact(ServiceId::new(ALICE)),
                target: MessageId {
                    author: ServiceId::new(ME),
                    sent_at_ms: 80
                },
                from: ServiceId::new(ALICE),
                emoji: "🐈".into(),
                remove: false,
            })
        );
    }

    /// An edit can only rewrite its sender's own message, so the target's author
    /// and `from` must both be the authenticated sender; the store's
    /// author check rests on that.
    #[test]
    fn edit_targets_and_comes_from_its_authenticated_sender() {
        let content = content_from(
            ALICE,
            ContentBody::EditMessage(EditMessage {
                target_sent_timestamp: Some(40),
                data_message: Some(DataMessage {
                    body: Some("fixed typo".into()),
                    timestamp: Some(41),
                    ..Default::default()
                }),
            }),
        );
        let alice = ServiceId::new(ALICE);
        assert_eq!(
            incoming(&content, ServiceId::new(ME)),
            Some(Incoming::Edit {
                conversation: Recipient::Contact(alice),
                target: MessageId {
                    author: alice,
                    sent_at_ms: 40
                },
                from: alice,
                body: "fixed typo".into(),
            })
        );
    }

    /// Delete-for-everyone erases only the sender's own message, so its target
    /// author is the sender, never a guess from the timestamp alone.
    #[test]
    fn delete_targets_and_comes_from_its_authenticated_sender() {
        let content = content_from(
            ALICE,
            ContentBody::DataMessage(DataMessage {
                timestamp: Some(60),
                delete: Some(data_message::Delete {
                    target_sent_timestamp: Some(50),
                }),
                ..Default::default()
            }),
        );
        let alice = ServiceId::new(ALICE);
        assert_eq!(
            incoming(&content, ServiceId::new(ME)),
            Some(Incoming::Delete {
                conversation: Recipient::Contact(alice),
                target: MessageId {
                    author: alice,
                    sent_at_ms: 50
                },
                from: alice,
            })
        );
    }

    /// A group admin deleting someone else's message must arrive as an admin
    /// delete naming its group, with the admin as `from` and the target keeping
    /// its real author, so capture can check `from` against the group's admins.
    /// Outside a group there are no admins, so it carries nothing to apply.
    #[test]
    fn admin_delete_arrives_as_an_admin_delete_only_in_its_group() {
        let master_key = [6u8; 32];
        let admin_delete = |group_v2| {
            content_from(
                ALICE,
                ContentBody::DataMessage(DataMessage {
                    timestamp: Some(61),
                    group_v2,
                    admin_delete: Some(data_message::AdminDelete {
                        target_author_aci_binary: Some(ME.as_bytes().to_vec()),
                        target_sent_timestamp: Some(51),
                    }),
                    ..Default::default()
                }),
            )
        };
        let in_group = admin_delete(Some(GroupContextV2 {
            master_key: Some(master_key.to_vec()),
            revision: Some(1),
            group_change: None,
        }));
        assert_eq!(
            incoming(&in_group, ServiceId::new(ME)),
            Some(Incoming::AdminDelete {
                group: group_id(master_key),
                target: MessageId {
                    author: ServiceId::new(ME),
                    sent_at_ms: 51
                },
                from: ServiceId::new(ALICE),
            })
        );
        assert_eq!(incoming(&admin_delete(None), ServiceId::new(ME)), None);
    }

    /// A read receipt from a contact marks the account's own messages read;
    /// delivery receipts carry nothing the agent keeps.
    #[test]
    fn read_receipt_targets_own_messages_and_delivery_is_ignored() {
        let receipt = |kind: receipt_message::Type| {
            content_from(
                ALICE,
                ContentBody::ReceiptMessage(ReceiptMessage {
                    r#type: Some(kind.into()),
                    timestamp: vec![10, 11],
                }),
            )
        };
        let me = ServiceId::new(ME);
        assert_eq!(
            incoming(&receipt(receipt_message::Type::Read), me),
            Some(Incoming::Read {
                targets: vec![
                    MessageId {
                        author: me,
                        sent_at_ms: 10
                    },
                    MessageId {
                        author: me,
                        sent_at_ms: 11
                    },
                ]
            })
        );
        assert_eq!(
            incoming(&receipt(receipt_message::Type::Delivery), me),
            None
        );
    }

    /// Reading a chat on the phone syncs here; the read messages are the
    /// original senders', not the account's.
    #[test]
    fn read_synced_from_own_phone_targets_original_senders() {
        let content = content_from(
            ME,
            ContentBody::SynchronizeMessage(SyncMessage {
                read: vec![sync_message::Read {
                    sender_aci: None,
                    sender_aci_binary: Some(ALICE.as_bytes().to_vec()),
                    timestamp: Some(33),
                }],
                ..Default::default()
            }),
        );
        assert_eq!(
            incoming(&content, ServiceId::new(ME)),
            Some(Incoming::Read {
                targets: vec![MessageId {
                    author: ServiceId::new(ALICE),
                    sent_at_ms: 33
                }]
            })
        );
    }

    /// A reply must point at the quoted message, or threads flatten into
    /// unrelated lines in the history.
    #[test]
    fn reply_carries_the_quoted_message_id() {
        let content = content_from(
            ALICE,
            ContentBody::DataMessage(DataMessage {
                body: Some("agreed".into()),
                timestamp: Some(21),
                quote: Some(data_message::Quote {
                    id: Some(20),
                    author_aci_binary: Some(ME.as_bytes().to_vec()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );
        let Some(Incoming::Message(message)) = incoming(&content, ServiceId::new(ME)) else {
            panic!("not a message");
        };
        assert_eq!(
            message.quote,
            Some(MessageId {
                author: ServiceId::new(ME),
                sent_at_ms: 20
            })
        );
    }

    proptest! {
        /// An attachment's id is all the agent keeps; it must bring back the
        /// exact pointer, keys and digest included, or the download fails its
        /// checksum.
        #[test]
        fn attachment_id_round_trips_to_its_pointer(
            key in proptest::collection::vec(any::<u8>(), 64),
            digest in proptest::collection::vec(any::<u8>(), 32),
            cdn_key in "[A-Za-z0-9_-]{1,40}",
            cdn_number in any::<u32>(),
            size in any::<u32>(),
            file_name in proptest::option::of(".{0,20}"),
        ) {
            let pointer = AttachmentPointer {
                content_type: Some("image/png".into()),
                key: Some(key),
                digest: Some(digest),
                size: Some(size),
                file_name,
                cdn_number: Some(cdn_number),
                attachment_identifier: Some(attachment_pointer::AttachmentIdentifier::CdnKey(cdn_key)),
                ..Default::default()
            };
            let reference = attachment_ref(&pointer);
            prop_assert_eq!(reference.size, u64::from(size));
            prop_assert_eq!(&reference.file_name, &pointer.file_name);
            let back = attachment_pointer(&reference).expect("own id decodes");
            prop_assert_eq!(back, pointer);
        }
    }

    /// Every attachment on a message must reach the agent, or a photo sent with
    /// a caption would arrive as the caption alone.
    #[test]
    fn message_lists_every_attachment() {
        let photo = AttachmentPointer {
            content_type: Some("image/jpeg".into()),
            size: Some(1234),
            ..Default::default()
        };
        let content = content_from(
            ALICE,
            ContentBody::DataMessage(DataMessage {
                timestamp: Some(70),
                attachments: vec![photo.clone(), photo.clone()],
                ..Default::default()
            }),
        );
        let Some(Incoming::Message(message)) = incoming(&content, ServiceId::new(ME)) else {
            panic!("not a message");
        };
        assert_eq!(
            message.attachments,
            vec![attachment_ref(&photo), attachment_ref(&photo)]
        );
    }

    proptest! {
        /// Attachment ids come back from MCP clients; garbage must be an error,
        /// never a panic in the agent.
        #[test]
        fn attachment_id_garbage_is_an_error_not_a_panic(id in ".*") {
            let _ = attachment_pointer(&AttachmentRef {
                id,
                content_type: String::new(),
                file_name: None,
                size: 0,
            });
        }
    }

    fn message_id_by(author: Uuid) -> impl Strategy<Value = MessageId> {
        (1u64..u64::MAX / 2).prop_map(move |sent_at_ms| MessageId {
            author: ServiceId::new(author),
            sent_at_ms,
        })
    }

    /// A ref as `upload` returns it: the id encodes a full pointer.
    fn uploaded() -> impl Strategy<Value = AttachmentRef> {
        (
            proptest::collection::vec(any::<u8>(), 64),
            proptest::collection::vec(any::<u8>(), 32),
            "[A-Za-z0-9_-]{1,40}",
            any::<u32>(),
            proptest::option::of("[a-z]{1,8}\\.png"),
        )
            .prop_map(|(key, digest, cdn_key, size, file_name)| {
                attachment_ref(&AttachmentPointer {
                    content_type: Some("image/png".into()),
                    key: Some(key),
                    digest: Some(digest),
                    size: Some(size),
                    file_name,
                    cdn_number: Some(3),
                    attachment_identifier: Some(attachment_pointer::AttachmentIdentifier::CdnKey(
                        cdn_key,
                    )),
                    ..Default::default()
                })
            })
    }

    fn sendable() -> impl Strategy<Value = Outgoing> {
        prop_oneof![
            (
                ".{0,40}",
                proptest::option::of(message_id_by(ME)),
                proptest::collection::vec(uploaded(), 0..3),
            )
                .prop_map(|(body, quote, attachments)| Outgoing::Text {
                    body,
                    quote,
                    attachments,
                }),
            (message_id_by(ME), ".{1,4}", any::<bool>()).prop_map(|(target, emoji, remove)| {
                Outgoing::Reaction {
                    target,
                    emoji,
                    remove,
                }
            }),
            (message_id_by(ALICE), ".{0,40}")
                .prop_map(|(target, body)| Outgoing::Edit { target, body }),
            message_id_by(ALICE).prop_map(|target| Outgoing::Delete { target }),
            message_id_by(ME).prop_map(|target| Outgoing::AdminDelete { target }),
        ]
    }

    proptest! {
        /// What this backend sends must read back as the same thing when it
        /// arrives, in a direct chat or a group; otherwise the phone and the
        /// agent's history disagree about what was said.
        #[test]
        fn sent_content_reads_back_as_the_same_event(
            message in sendable(),
            in_group in any::<bool>(),
            sent_at_ms in 1u64..u64::MAX / 2,
        ) {
            let master_key = [3u8; 32];
            let group = in_group.then(|| GroupContextV2 {
                master_key: Some(master_key.to_vec()),
                revision: Some(4),
                group_change: None,
            });
            let body = content_body(&message, sent_at_ms, group.as_ref());
            if matches!(message, Outgoing::AdminDelete { .. }) && !in_group {
                prop_assert!(body.is_err(), "an admin delete went to a person");
                return Ok(());
            }
            let body = body.expect("generated variants convert");
            let alice = ServiceId::new(ALICE);
            let conversation = if in_group {
                Recipient::Group(group_id(master_key))
            } else {
                Recipient::Contact(alice)
            };
            let got = incoming(&content_from(ALICE, body), ServiceId::new(ME));
            let expected = match message {
                Outgoing::Text { body, quote, attachments } => {
                    let Some(Incoming::Message(got)) = got else {
                        return Err(TestCaseError::fail(format!("text read back as {got:?}")));
                    };
                    prop_assert_eq!(got.id, MessageId { author: alice, sent_at_ms });
                    prop_assert_eq!(got.conversation, conversation);
                    prop_assert_eq!(got.body, Some(body));
                    prop_assert_eq!(got.quote, quote);
                    prop_assert_eq!(got.attachments, attachments);
                    return Ok(());
                }
                Outgoing::Reaction { target, emoji, remove } => Incoming::Reaction { conversation, target, from: alice, emoji, remove },
                Outgoing::Edit { target, body } => Incoming::Edit { conversation, target, from: alice, body },
                Outgoing::Delete { target } => Incoming::Delete { conversation, target, from: alice },
                Outgoing::AdminDelete { target } => Incoming::AdminDelete { group: group_id(master_key), target, from: alice },
                other => unreachable!("not generated: {other:?}"),
            };
            prop_assert_eq!(got, Some(expected));
        }
    }

    /// The DataMessage `content_body` puts on the wire for `message`.
    fn data_of(message: &Outgoing) -> DataMessage {
        match content_body(message, 9, None).expect("converts") {
            ContentBody::DataMessage(data) => data,
            other => panic!("{message:?} went out as {other:?}"),
        }
    }

    proptest! {
        /// A pin names its target by author and timestamp, as Signal's apps
        /// match it, for the duration asked; no duration pins until unpinned,
        /// which the wire spells as "forever", not as a missing duration.
        #[test]
        fn a_pin_names_its_target_and_no_duration_means_forever(
            target in message_id_by(ALICE),
            duration_secs in proptest::option::of(1u32..),
        ) {
            let duration_secs = duration_secs.and_then(std::num::NonZeroU32::new);
            let pin = data_of(&Outgoing::Pin { target, duration_secs })
                .pin_message
                .expect("a pin message");
            prop_assert_eq!(pin.target_author_aci_binary, Some(ALICE.as_bytes().to_vec()));
            prop_assert_eq!(pin.target_sent_timestamp, Some(target.sent_at_ms));
            let expected = match duration_secs {
                Some(secs) => data_message::pin_message::PinDuration::PinDurationSeconds(secs.get()),
                None => data_message::pin_message::PinDuration::PinDurationForever(true),
            };
            prop_assert_eq!(pin.pin_duration, Some(expected));
        }
    }

    proptest! {
        /// Signal's apps change a chat's timer only on a message flagged as a
        /// timer update; without the flag the seconds are just the timer the
        /// message itself expires under, and the chat setting stays put.
        #[test]
        fn a_timer_update_carries_the_seconds_and_the_update_flag(seconds in any::<u32>()) {
            let update = data_of(&Outgoing::ExpirationTimer { seconds });
            prop_assert_eq!(update.expire_timer, Some(seconds));
            prop_assert_eq!(
                update.flags,
                Some(data_message::Flags::ExpirationTimerUpdate as u32)
            );
        }
    }

    proptest! {
        /// Accepting a request must hand the requester our profile key in a
        /// message flagged as a profile-key update, or their app keeps
        /// showing us as a stranger.
        #[test]
        fn accepting_a_request_sends_a_flagged_profile_key_update(sent_at_ms in 1u64..) {
            let ContentBody::DataMessage(update) = profile_key_update(sent_at_ms) else {
                panic!("the profile key update is not a data message");
            };
            prop_assert_eq!(update.flags, Some(data_message::Flags::ProfileKeyUpdate as u32));
            prop_assert_eq!(update.timestamp, Some(sent_at_ms));
            prop_assert_eq!(update.body, None);
        }
    }

    proptest! {
        /// Our other devices match the answer to a conversation by the
        /// requester's ACI, and must see the same answer we gave.
        #[test]
        fn the_request_answer_syncs_to_own_devices_naming_the_requester(
            accept in any::<bool>(),
        ) {
            let response = if accept { RequestResponse::Accept } else { RequestResponse::Delete };
            let ContentBody::SynchronizeMessage(sync) =
                request_response_sync(ServiceId::new(ALICE), response).expect("converts")
            else {
                panic!("the answer is not a sync message");
            };
            let Some(SyncContent::MessageRequestResponse(answer)) = sync.content else {
                panic!("the sync carries no message request response");
            };
            prop_assert_eq!(answer.thread_aci_binary, Some(ALICE.as_bytes().to_vec()));
            let expected = if accept {
                sync_message::message_request_response::Type::Accept
            } else {
                sync_message::message_request_response::Type::Delete
            };
            prop_assert_eq!(answer.r#type, Some(expected as i32));
        }
    }

    /// Signal replaces the whole profile on upload, so a change to one field
    /// must carry the others over unchanged, an empty string must remove an
    /// optional field, and a name with no given name must never be uploaded.
    #[test]
    fn a_profile_update_keeps_unnamed_fields_and_empty_removes_one() {
        let current = Profile {
            name: Some(ProfileName {
                given_name: "Ada".into(),
                family_name: Some("Lovelace".into()),
            }),
            about: Some("engines".into()),
            about_emoji: Some("⚙️".into()),
            avatar: None,
            unrestricted_unidentified_access: false,
        };
        let (name, about, emoji) = merged_profile(
            current.clone(),
            ProfileUpdate {
                about: Some("looms".into()),
                about_emoji: Some(String::new()),
                ..ProfileUpdate::default()
            },
        )
        .expect("merges");
        assert_eq!(
            (name.given_name.as_str(), name.family_name.as_deref()),
            ("Ada", Some("Lovelace"))
        );
        assert_eq!((about.as_deref(), emoji), (Some("looms"), None));
        for blank in ["", "  "] {
            let update = ProfileUpdate {
                given_name: Some(blank.into()),
                ..ProfileUpdate::default()
            };
            assert!(
                merged_profile(current.clone(), update).is_err(),
                "{blank:?}"
            );
        }
        let nameless = Profile {
            name: None,
            ..current
        };
        let update = ProfileUpdate {
            about: Some("x".into()),
            ..ProfileUpdate::default()
        };
        assert!(
            merged_profile(nameless, update).is_err(),
            "uploaded without a name"
        );
    }

    /// A group's timer lives in the group's state, so a timer update sent as
    /// a group message would be ignored by every member's app.
    #[test]
    fn a_timer_update_never_goes_to_a_group() {
        let group = GroupContextV2 {
            master_key: Some(vec![5; 32]),
            revision: Some(1),
            group_change: None,
        };
        let result = content_body(&Outgoing::ExpirationTimer { seconds: 60 }, 9, Some(&group));
        assert!(result.is_err(), "group timer update converted: {result:?}");
    }

    proptest! {
        /// An unpin must name the same target a pin did, or the pin stays up.
        #[test]
        fn an_unpin_names_its_target_like_the_pin(target in message_id_by(ALICE)) {
            let unpin = data_of(&Outgoing::Unpin { target })
                .unpin_message
                .expect("an unpin message");
            let pin = data_of(&Outgoing::Pin { target, duration_secs: None })
                .pin_message
                .expect("a pin message");
            prop_assert_eq!(unpin.target_author_aci_binary, pin.target_author_aci_binary);
            prop_assert_eq!(unpin.target_sent_timestamp, pin.target_sent_timestamp);
        }
    }

    proptest! {
        /// A poll goes out with its question, its options in the order given
        /// (votes are indexes into them), and whether several may be chosen.
        #[test]
        fn a_poll_keeps_its_question_and_option_order(
            question in ".{1,40}",
            options in proptest::collection::vec(".{1,20}", 2..6),
            multi_select in any::<bool>(),
        ) {
            let poll = data_of(&Outgoing::PollCreate {
                question: question.clone(),
                options: options.clone(),
                multi_select,
            })
            .poll_create
            .expect("a poll");
            prop_assert_eq!(poll.question, Some(question));
            prop_assert_eq!(poll.options, options);
            prop_assert_eq!(poll.allow_multiple, Some(multi_select));
        }
    }

    proptest! {
        /// A vote names the poll by its author and timestamp and carries the
        /// caller's counter untouched; receivers keep only the highest count,
        /// so a rewritten counter would make a later vote lose to an earlier one.
        #[test]
        fn a_vote_names_its_poll_and_keeps_its_count(
            poll in message_id_by(ALICE),
            option_indexes in proptest::collection::vec(0u32..10, 0..4),
            vote_count in 1u32..,
        ) {
            let vote = data_of(&Outgoing::PollVote {
                poll,
                option_indexes: option_indexes.clone(),
                vote_count,
            })
            .poll_vote
            .expect("a vote");
            prop_assert_eq!(vote.target_author_aci_binary, Some(ALICE.as_bytes().to_vec()));
            prop_assert_eq!(vote.target_sent_timestamp, Some(poll.sent_at_ms));
            prop_assert_eq!(vote.option_indexes, option_indexes);
            prop_assert_eq!(vote.vote_count, Some(vote_count));
        }
    }

    proptest! {
        /// Ending a poll names it by timestamp alone; receivers take the
        /// sender as its author.
        #[test]
        fn ending_a_poll_names_its_timestamp(poll in message_id_by(ME)) {
            let end = data_of(&Outgoing::PollTerminate { poll })
                .poll_terminate
                .expect("a poll end");
            prop_assert_eq!(end.target_sent_timestamp, Some(poll.sent_at_ms));
        }
    }

    /// A sticker goes out naming its pack and key, so receivers with the pack
    /// show it from there, and with the uploaded image for those without.
    #[test]
    fn a_sticker_names_its_pack_and_carries_its_image() {
        let pack = StickerPackId::new([1; 16]);
        let image = AttachmentPointer {
            content_type: Some("image/webp".into()),
            size: Some(10),
            ..Default::default()
        };
        let ContentBody::DataMessage(data) =
            sticker_body(pack, &[2; 32], 7, Some("🐈"), image.clone(), 9, None)
        else {
            panic!("a sticker is not a data message");
        };
        let sticker = data.sticker.expect("a sticker");
        assert_eq!(sticker.pack_id, Some(vec![1; 16]));
        assert_eq!(sticker.pack_key, Some(vec![2; 32]));
        assert_eq!(sticker.sticker_id, Some(7));
        assert_eq!(sticker.emoji.as_deref(), Some("🐈"));
        assert_eq!(sticker.data, Some(image));
        assert_eq!(data.timestamp, Some(9));
    }

    /// Receivers decide how to show an attachment by its content type, so a
    /// photo must go out as an image, under its own file name; a type nobody
    /// recognizes goes out as a plain file rather than a guess.
    #[test]
    fn an_upload_carries_its_file_name_and_a_content_type_from_it() {
        let cases = [
            ("/send/cat.png", "image/png"),
            ("/send/Notes.PDF", "application/pdf"),
            ("/send/unknown.zzz9", "application/octet-stream"),
        ];
        for (path, content_type) in cases {
            let spec = attachment_spec(std::path::Path::new(path), 42);
            assert_eq!(spec.content_type, content_type, "{path}");
            assert_eq!(spec.length, 42, "{path}");
            assert_eq!(spec.file_name.as_deref(), path.rsplit('/').next(), "{path}");
        }
    }

    proptest! {
        /// A linked device keeps its id (1 is the primary), its decrypted
        /// name, and both times in milliseconds, as list_devices shows them.
        #[test]
        fn a_device_keeps_its_id_name_and_times_in_ms(
            id in 1u8..=127,
            name in proptest::option::of(".{0,20}"),
            created_at_ms in 0i64..4_000_000_000_000,
            last_seen_at_ms in 0i64..4_000_000_000_000,
        ) {
            let at = |ms| Utc.timestamp_millis_opt(ms).single().expect("valid time");
            let info = DeviceInfo {
                id: DeviceId::new(id).expect("valid device id"),
                registration_id: 1,
                name: name.clone(),
                created_at: at(created_at_ms),
                last_seen: at(last_seen_at_ms),
            };
            prop_assert_eq!(
                device(&info),
                crate::Device {
                    id: u32::from(id),
                    name,
                    created_at_ms: created_at_ms as u64,
                    last_seen_at_ms: last_seen_at_ms as u64,
                }
            );
        }
    }

    fn identity_key(seed: u8) -> protocol::IdentityKey {
        let mut bytes = [seed; 33];
        bytes[0] = 0x05;
        protocol::IdentityKey::decode(&bytes).expect("a curve25519 public key")
    }

    proptest! {
        // Each case hashes 5200 rounds four times; a few cases cover it.
        #![proptest_config(ProptestConfig::with_cases(8))]
        /// A safety number is compared by reading it aloud between two phones,
        /// so both sides must compute the same 60 digits, and a new key on
        /// either side must change them.
        #[test]
        fn both_sides_see_the_same_safety_number_and_a_new_key_changes_it(
            mine in 1u8..=255,
            theirs in 1u8..=255,
            replaced in 1u8..=255,
        ) {
            prop_assume!(replaced != theirs);
            let (me, alice) = (ServiceId::new(ME), ServiceId::new(ALICE));
            let ours = safety_number(me, &identity_key(mine), alice, &identity_key(theirs))
                .expect("computes");
            let from_alice = safety_number(alice, &identity_key(theirs), me, &identity_key(mine))
                .expect("computes");
            prop_assert_eq!(ours.len(), 60);
            prop_assert!(ours.bytes().all(|b| b.is_ascii_digit()), "{}", ours);
            prop_assert_eq!(&ours, &from_alice);
            let after = safety_number(me, &identity_key(mine), alice, &identity_key(replaced))
                .expect("computes");
            prop_assert_ne!(ours, after);
        }
    }

    /// Typing in a group names the group by its public id; group members' apps
    /// match on that, and the master key must never go out bare.
    #[test]
    fn group_typing_names_the_public_group_id() {
        let master_key = [5u8; 32];
        let group = GroupContextV2 {
            master_key: Some(master_key.to_vec()),
            revision: Some(1),
            group_change: None,
        };
        let ContentBody::TypingMessage(typing) =
            content_body(&Outgoing::Typing { started: true }, 9, Some(&group)).expect("converts")
        else {
            panic!("typing is not a typing message");
        };
        assert_eq!(
            typing.group_id,
            Some(group_id(master_key).as_bytes().to_vec())
        );
    }

    /// Signal stores an unnamed contact as an empty name; tools must see no
    /// name rather than a blank one, and the number in E.164.
    #[test]
    fn unnamed_contact_has_no_name_and_an_e164_number() {
        let contact = presage::model::contacts::Contact {
            uuid: ALICE,
            phone_number: Some("+15550001111".parse().expect("valid number")),
            name: String::new(),
            verified: Default::default(),
            profile_key: Vec::new(),
            expire_timer: 0,
            expire_timer_version: 2,
            inbox_position: 0,
            avatar: None,
        };
        assert_eq!(
            contact_from(&contact),
            crate::Contact {
                service_id: ServiceId::new(ALICE),
                name: None,
                phone_number: Some("+15550001111".into()),
            }
        );
    }

    /// A group lists its members by ACI under its public id, so tools can
    /// address it without ever seeing the master key.
    #[test]
    fn group_lists_members_under_its_public_id() {
        let master_key = [8u8; 32];
        let member = |uuid: Uuid, role| presage::model::groups::Member {
            aci: aci(uuid),
            role,
            profile_key: presage::libsignal_service::prelude::ProfileKey::create([0; 32]),
            joined_at_revision: 0,
            label: None,
            label_emoji: None,
        };
        let group = presage::model::groups::Group {
            title: "cats".into(),
            avatar: String::new(),
            disappearing_messages_timer: None,
            access_control: None,
            revision: 3,
            members: vec![
                member(ALICE, Role::Administrator),
                member(ME, Role::Default),
            ],
            pending_members: Vec::new(),
            requesting_members: Vec::new(),
            invite_link_password: Vec::new(),
            description: None,
        };
        assert_eq!(
            group_from(master_key, &group),
            crate::Group {
                id: group_id(master_key),
                title: "cats".into(),
                members: vec![ServiceId::new(ALICE), ServiceId::new(ME)],
                admins: vec![ServiceId::new(ALICE)],
            }
        );
    }
}
