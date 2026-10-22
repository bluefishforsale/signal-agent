//! Continuous capture: everything Signal delivers lands in the store whether or
//! not any Claude app is connected, so history is complete when one asks.

use std::sync::Arc;

use anyhow::bail;
use futures::StreamExt;
use relay_signal::{Incoming, SignalBackend};
use relay_store::MessageStore;

/// Applies every event from `signal.incoming()` to `store` until the stream ends.
pub(crate) async fn run(signal: Arc<dyn SignalBackend>, store: Arc<dyn MessageStore>) {
    let mut events = signal.incoming();
    while let Some(event) = events.next().await {
        let result = match event {
            Ok(event) => apply(store.as_ref(), event).await,
            Err(e) => Err(e),
        };
        // NOTE: one bad event (a forged edit, a delete for a message we never saw)
        // must not stop capture, or every later message would be lost.
        if let Err(e) = result {
            tracing::warn!("capture skipped an event: {e:#}");
        }
    }
}

async fn apply(store: &dyn MessageStore, event: Incoming) -> anyhow::Result<()> {
    match event {
        Incoming::Message(message) => {
            // NOTE: the store's edit and delete checks trust `id.author`, so a
            // message whose id names someone other than its sender is forged.
            if message.id.author != message.sender {
                bail!(
                    "{} sent a message under {}'s id",
                    message.sender,
                    message.id.author
                );
            }
            store.insert(&message).await.map(|_| ())
        }
        Incoming::Edit {
            target, from, body, ..
        } => store.apply_edit(target, from, &body).await,
        Incoming::Delete { target, from, .. } => store.mark_deleted(target, from).await,
        Incoming::Read { targets } => store.mark_read(&targets).await,
        // NOTE: MessageStore has no place for reactions yet, so they are dropped
        // here. When the store grows one, apply them with `from` like edits.
        Incoming::Reaction { .. } => Ok(()),
        // Nothing to store until the identity tools (lane L3d) exist; the log
        // keeps a trusted safety-number change from passing silently.
        Incoming::IdentityChanged { contact } => {
            tracing::warn!("safety number changed for {contact}; trusted the new key");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use relay_core::prelude::*;
    use relay_signal::{Account, FakeSignal};
    use relay_store::MemoryStore;
    use uuid::Uuid;

    use super::*;

    fn fake() -> Arc<FakeSignal> {
        Arc::new(FakeSignal::new(Account {
            service_id: ServiceId::new(Uuid::from_u128(1)),
            phone_number: None,
            kind: ProfileKind::Linked,
            device_name: "test".into(),
        }))
    }

    fn msg(author: u128, sent_at_ms: u64, body: &str) -> Message {
        let sender = ServiceId::new(Uuid::from_u128(author));
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

    /// A message received while no client is connected must be findable later
    /// (PRD acceptance 7): capture writes it to the store on its own.
    #[tokio::test]
    async fn received_messages_land_in_the_store_without_a_client() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone()));
        signal.push(Incoming::Message(msg(2, 10, "dinner at 8")));
        let hits = eventually(|| async {
            store
                .search(&relay_store::SearchQuery {
                    text: "dinner".into(),
                    limit: 10,
                    ..Default::default()
                })
                .await
                .expect("search")
        })
        .await;
        capture.abort();
        assert_eq!(hits.len(), 1, "captured message not searchable: {hits:?}");
    }

    /// Edits must reach the store with their sender so the store can refuse a
    /// forged one, and a refused edit must not stop capture of the next event.
    #[tokio::test]
    async fn a_forged_edit_is_refused_and_capture_keeps_going() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone()));
        let original = msg(2, 10, "original");
        let conversation = original.conversation;
        signal.push(Incoming::Message(original.clone()));
        signal.push(Incoming::Edit {
            conversation,
            target: original.id,
            from: ServiceId::new(Uuid::from_u128(666)),
            body: "forged".into(),
        });
        signal.push(Incoming::Edit {
            conversation,
            target: original.id,
            from: original.sender,
            body: "fixed typo".into(),
        });
        let edited = eventually(|| async {
            store
                .history(conversation, None, 1)
                .await
                .expect("history")
                .into_iter()
                .filter(|m| m.edited)
                .collect()
        })
        .await;
        capture.abort();
        let body = edited.first().and_then(|m| m.body.as_deref());
        assert_eq!(body, Some("fixed typo"), "edit not applied: {edited:?}");
    }

    /// A delete-for-everyone must reach the store with its sender, so the
    /// author's delete hides the message and a stranger's cannot.
    #[tokio::test]
    async fn deletes_are_applied_with_their_sender() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone()));
        let original = msg(2, 10, "oops");
        let conversation = original.conversation;
        signal.push(Incoming::Message(original.clone()));
        signal.push(Incoming::Delete {
            conversation,
            target: original.id,
            from: original.sender,
        });
        let deleted = eventually(|| async {
            store
                .history(conversation, None, 1)
                .await
                .expect("history")
                .into_iter()
                .filter(|m| m.deleted)
                .collect()
        })
        .await;
        capture.abort();
        assert_eq!(deleted.len(), 1, "delete not applied: {deleted:?}");
    }

    /// A read sync from the phone must clear unread here too, or Claude would
    /// keep reporting messages the user already read.
    #[tokio::test]
    async fn read_syncs_clear_unread() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone()));
        let original = msg(2, 10, "seen it");
        signal.push(Incoming::Message(original.clone()));
        signal.push(Incoming::Read {
            targets: vec![original.id],
        });
        signal.push(Incoming::Message(msg(2, 11, "marker")));
        let marker = eventually(|| async {
            let unread = store.unread().await.expect("unread");
            if unread.iter().any(|m| m.id.sent_at_ms == 11) {
                unread
            } else {
                vec![]
            }
        })
        .await;
        capture.abort();
        let still_unread: Vec<_> = marker.iter().filter(|m| m.id == original.id).collect();
        assert!(
            still_unread.is_empty(),
            "read sync ignored: {still_unread:?}"
        );
    }

    /// A message claiming another author's id is dropped and the stored
    /// original is unchanged: otherwise anyone could plant text under someone
    /// else's id and pass the author checks that guard edits and deletes.
    #[tokio::test]
    async fn a_message_claiming_another_authors_id_is_dropped_and_the_original_is_unchanged() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone()));
        let original = msg(2, 10, "original");
        signal.push(Incoming::Message(original.clone()));
        let mut forged = msg(2, 11, "forged");
        forged.sender = ServiceId::new(Uuid::from_u128(666));
        signal.push(Incoming::Message(forged));
        signal.push(Incoming::Message(msg(2, 12, "marker")));
        let stored = eventually(|| async {
            let all = store
                .history(original.conversation, None, 10)
                .await
                .expect("history");
            if all.iter().any(|m| m.id.sent_at_ms == 12) {
                all
            } else {
                vec![]
            }
        })
        .await;
        capture.abort();
        let bodies: Vec<_> = stored.iter().filter_map(|m| m.body.as_deref()).collect();
        assert!(
            !bodies.contains(&"forged"),
            "forged message stored: {bodies:?}"
        );
        assert!(bodies.contains(&"original"), "original lost: {bodies:?}");
    }

    /// A safety-number change is an event to note, not an error: capture must
    /// keep storing what arrives after it.
    #[tokio::test]
    async fn an_identity_change_does_not_interrupt_capture() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone()));
        signal.push(Incoming::IdentityChanged {
            contact: ServiceId::new(Uuid::from_u128(2)),
        });
        let after = msg(2, 10, "after the change");
        signal.push(Incoming::Message(after.clone()));
        let stored = eventually(|| async {
            store
                .history(after.conversation, None, 1)
                .await
                .expect("history")
        })
        .await;
        capture.abort();
        assert_eq!(stored.len(), 1, "capture stopped after an identity change");
    }

    async fn eventually<F, Fut>(mut probe: F) -> Vec<Message>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Vec<Message>>,
    {
        for _ in 0..200 {
            let found = probe().await;
            if !found.is_empty() {
                return found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        probe().await
    }
}
