//! The only place presage and libsignal types meet relay-core types, so the
//! protocol's shape changes in one file when presage moves.

use presage::{
    libsignal_service::{
        content::{Content, ContentBody},
        protocol,
        zkgroup::groups::{GroupMasterKey, GroupSecretParams},
    },
    proto::{
        DataMessage, SyncMessage,
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
    match &content.body {
        ContentBody::DataMessage(data) => Some(Incoming::Message(message(
            data,
            sender,
            conversation,
            me,
            content,
        )?)),
        ContentBody::SynchronizeMessage(SyncMessage {
            content:
                Some(SyncContent::Sent(sync_message::Sent {
                    message: Some(data),
                    ..
                })),
            ..
        }) => Some(Incoming::Message(message(
            data,
            sender,
            conversation,
            me,
            content,
        )?)),
        _ => None,
    }
}

fn message(
    data: &DataMessage,
    sender: ServiceId,
    conversation: Recipient,
    me: ServiceId,
    content: &Content,
) -> Option<Message> {
    Some(Message {
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
        quote: None,
        edited: false,
        deleted: false,
    })
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
        proto::GroupContextV2,
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
}
