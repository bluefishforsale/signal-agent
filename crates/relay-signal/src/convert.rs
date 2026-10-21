//! The only place presage and libsignal types meet relay-core types, so the
//! protocol's shape changes in one file when presage moves.

use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use presage::libsignal_service::prelude::ProtobufMessage;
use presage::{
    libsignal_service::{
        content::{Content, ContentBody},
        protocol,
        zkgroup::groups::{GroupMasterKey, GroupSecretParams},
    },
    proto::{
        AttachmentPointer, DataMessage, EditMessage, GroupContextV2, ReceiptMessage, SyncMessage,
        TypingMessage, data_message, receipt_message,
        sync_message::{self, Content as SyncContent},
        typing_message,
    },
    store::Thread,
};
use relay_core::prelude::*;

use crate::{Incoming, Outgoing};

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
        return Some(Incoming::Delete {
            conversation,
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

/// What to put on the wire for `message`, sent at `sent_at_ms`, carrying
/// `group` when it goes to a group.
pub(crate) fn content_body(
    message: &Outgoing,
    sent_at_ms: u64,
    group: Option<&GroupContextV2>,
) -> ContentBody {
    let data = |message: DataMessage| DataMessage {
        timestamp: Some(sent_at_ms),
        group_v2: group.cloned(),
        ..message
    };
    match message {
        Outgoing::Text { body, quote } => ContentBody::DataMessage(data(DataMessage {
            body: Some(body.clone()),
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
    }
}

fn aci(id: ServiceId) -> protocol::Aci {
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

    /// A group admin deleting someone else's message must arrive as a delete
    /// whose `from` is the admin and whose target keeps its real author, so the
    /// store sees the mismatch instead of a forged self-delete.
    #[test]
    fn admin_delete_keeps_target_author_and_admin_as_from() {
        let content = content_from(
            ALICE,
            ContentBody::DataMessage(DataMessage {
                timestamp: Some(61),
                admin_delete: Some(data_message::AdminDelete {
                    target_author_aci_binary: Some(ME.as_bytes().to_vec()),
                    target_sent_timestamp: Some(51),
                }),
                ..Default::default()
            }),
        );
        assert_eq!(
            incoming(&content, ServiceId::new(ME)),
            Some(Incoming::Delete {
                conversation: Recipient::Contact(ServiceId::new(ALICE)),
                target: MessageId {
                    author: ServiceId::new(ME),
                    sent_at_ms: 51
                },
                from: ServiceId::new(ALICE),
            })
        );
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

    fn sendable() -> impl Strategy<Value = Outgoing> {
        prop_oneof![
            (".{0,40}", proptest::option::of(message_id_by(ME)))
                .prop_map(|(body, quote)| Outgoing::Text { body, quote }),
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
            let alice = ServiceId::new(ALICE);
            let conversation = if in_group {
                Recipient::Group(group_id(master_key))
            } else {
                Recipient::Contact(alice)
            };
            let got = incoming(&content_from(ALICE, body), ServiceId::new(ME));
            let expected = match message {
                Outgoing::Text { body, quote } => {
                    let Some(Incoming::Message(got)) = got else {
                        return Err(TestCaseError::fail(format!("text read back as {got:?}")));
                    };
                    prop_assert_eq!(got.id, MessageId { author: alice, sent_at_ms });
                    prop_assert_eq!(got.conversation, conversation);
                    prop_assert_eq!(got.body, Some(body));
                    prop_assert_eq!(got.quote, quote);
                    return Ok(());
                }
                Outgoing::Reaction { target, emoji, remove } => Incoming::Reaction { conversation, target, from: alice, emoji, remove },
                Outgoing::Edit { target, body } => Incoming::Edit { conversation, target, from: alice, body },
                Outgoing::Delete { target } => Incoming::Delete { conversation, target, from: alice },
                Outgoing::Typing { .. } | Outgoing::ReadReceipt { .. } => unreachable!("not generated"),
            };
            prop_assert_eq!(got, Some(expected));
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
            content_body(&Outgoing::Typing { started: true }, 9, Some(&group))
        else {
            panic!("typing is not a typing message");
        };
        assert_eq!(
            typing.group_id,
            Some(group_id(master_key).as_bytes().to_vec())
        );
    }
}
