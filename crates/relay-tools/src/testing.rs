//! Fixtures shared by every tool module's tests, so each batch tests against
//! the same fake account and store.

use std::sync::Arc;

use relay_core::prelude::*;
use relay_signal::{Account, Contact, FakeSignal};
use relay_store::MemoryStore;
use uuid::Uuid;

use crate::ToolContext;

pub const ME: u128 = 1;

pub fn sid(n: u128) -> ServiceId {
    ServiceId::new(Uuid::from_u128(n))
}

pub fn ctx(access: AccessMode, kind: ProfileKind) -> (ToolContext, Arc<FakeSignal>) {
    ctx_with(access, kind, vec![])
}

pub fn ctx_with(
    access: AccessMode,
    kind: ProfileKind,
    contacts: Vec<Contact>,
) -> (ToolContext, Arc<FakeSignal>) {
    let signal = Arc::new(
        FakeSignal::new(Account {
            service_id: sid(ME),
            phone_number: Some("+15550000001".into()),
            kind,
            device_name: "test".into(),
        })
        .with_contacts(contacts),
    );
    let ctx = ToolContext {
        store: Arc::new(MemoryStore::default()),
        signal: signal.clone(),
        access,
        kind,
        send_roots: Arc::default(),
        webhook: None,
    };
    (ctx, signal)
}

/// A message from `sender` in their DM conversation, sent at `sent_at_ms`.
pub fn incoming(sender: u128, sent_at_ms: u64, body: &str) -> Message {
    let sender = sid(sender);
    Message {
        id: MessageId {
            author: sender,
            sent_at_ms,
        },
        conversation: Recipient::Contact(sender),
        sender,
        from_me: false,
        received_at_ms: sent_at_ms,
        body: Some(body.to_owned()),
        attachments: vec![],
        quote: None,
        edited: false,
        deleted: false,
    }
}
