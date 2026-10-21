//! The only place presage and libsignal types meet relay-core types, so the
//! protocol's shape changes in one file when presage moves.

use presage::{
    libsignal_service::{
        content::{Content, ContentBody},
        protocol,
        zkgroup::groups::{GroupMasterKey, GroupSecretParams},
    },
    proto::{
        SyncMessage, receipt_message,
        sync_message::{self, Content as SyncContent},
    },
    store::Thread,
};
use relay_core::prelude::*;

use crate::Incoming;

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
        attachments: Vec::new(),
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
        proto::{DataMessage, EditMessage, GroupContextV2, ReceiptMessage, data_message},
        protocol::{Aci, DeviceId},
    };

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
}
