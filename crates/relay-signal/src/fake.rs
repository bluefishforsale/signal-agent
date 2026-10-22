//! An in-memory Signal for tests: records what was sent, delivers what a test
//! pushes, and never touches the network.

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, bail};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use relay_core::prelude::*;
use tokio::sync::mpsc;

use crate::{Account, Contact, Group, Incoming, Outgoing, RateLimited, SignalBackend};

/// A fake registered account. Build one with `FakeSignal::new`, seed it with
/// contacts, groups and attachments, then push incoming events at it.
pub struct FakeSignal {
    account: Account,
    state: Mutex<State>,
    clock_ms: AtomicU64,
    incoming_tx: mpsc::UnboundedSender<Incoming>,
    incoming_rx: Mutex<Option<mpsc::UnboundedReceiver<Incoming>>>,
}

#[derive(Default)]
struct State {
    contacts: Vec<Contact>,
    groups: Vec<Group>,
    attachments: HashMap<String, Vec<u8>>,
    sent: Vec<(Recipient, Outgoing)>,
    attempts: usize,
    rate_limit_next_send: Option<RateLimited>,
}

impl FakeSignal {
    pub fn new(account: Account) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        Self {
            account,
            state: Mutex::default(),
            clock_ms: AtomicU64::new(1_700_000_000_000),
            incoming_tx,
            incoming_rx: Mutex::new(Some(incoming_rx)),
        }
    }

    pub fn with_contacts(self, contacts: Vec<Contact>) -> Self {
        self.state().contacts = contacts;
        self
    }

    pub fn with_groups(self, groups: Vec<Group>) -> Self {
        self.state().groups = groups;
        self
    }

    pub fn with_attachment(self, id: impl Into<String>, bytes: Vec<u8>) -> Self {
        self.state().attachments.insert(id.into(), bytes);
        self
    }

    /// Delivers `event` to whoever is consuming `incoming()`.
    pub fn push(&self, event: Incoming) {
        // A dropped receiver just means no consumer yet in this test; nothing to report.
        let _ = self.incoming_tx.send(event);
    }

    /// Everything sent so far, in order.
    pub fn sent(&self) -> Vec<(Recipient, Outgoing)> {
        self.state().sent.clone()
    }

    /// Sends that reached Signal so far, whether it accepted them or not.
    pub fn attempts(&self) -> usize {
        self.state().attempts
    }

    /// Makes the next send that reaches Signal fail with `limited`, as Signal's
    /// rate limiting would.
    pub fn rate_limit_next_send(&self, limited: RateLimited) {
        self.state().rate_limit_next_send = Some(limited);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("fake signal state")
    }
}

#[async_trait]
impl SignalBackend for FakeSignal {
    async fn account(&self) -> anyhow::Result<Account> {
        Ok(self.account.clone())
    }

    async fn send(&self, to: Recipient, message: Outgoing) -> anyhow::Result<MessageId> {
        if let Recipient::Group(id) = to
            && !self.state().groups.iter().any(|g| g.id == id)
        {
            bail!("not a member of group {id}");
        }
        let mut state = self.state();
        state.attempts += 1;
        if let Some(limited) = state.rate_limit_next_send.take() {
            return Err(limited.into());
        }
        state.sent.push((to, message));
        drop(state);
        Ok(MessageId {
            author: self.account.service_id,
            sent_at_ms: self.clock_ms.fetch_add(1, Ordering::Relaxed),
        })
    }

    async fn contacts(&self) -> anyhow::Result<Vec<Contact>> {
        Ok(self.state().contacts.clone())
    }

    async fn groups(&self) -> anyhow::Result<Vec<Group>> {
        Ok(self.state().groups.clone())
    }

    async fn attachment(&self, attachment: &AttachmentRef) -> anyhow::Result<Vec<u8>> {
        self.state()
            .attachments
            .get(&attachment.id)
            .cloned()
            .with_context(|| format!("no attachment with id \"{}\"", attachment.id))
    }

    fn incoming(&self) -> BoxStream<'static, anyhow::Result<Incoming>> {
        let Some(rx) = self
            .incoming_rx
            .lock()
            .expect("fake signal receiver")
            .take()
        else {
            return futures::stream::once(async {
                Err(anyhow::anyhow!(
                    "incoming() already taken by another consumer"
                ))
            })
            .boxed();
        };
        tokio_stream_from(rx).map(Ok).boxed()
    }
}

fn tokio_stream_from<T: Send + 'static>(
    mut rx: mpsc::UnboundedReceiver<T>,
) -> BoxStream<'static, T> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn fake() -> FakeSignal {
        FakeSignal::new(Account {
            service_id: ServiceId::new(Uuid::from_u128(1)),
            phone_number: Some("+15550000001".into()),
            kind: ProfileKind::Linked,
            device_name: "test".into(),
        })
    }

    /// Two consumers of incoming() would split messages between them; the agent
    /// relies on exactly one, so a second call must fail loudly instead.
    #[tokio::test]
    async fn incoming_can_only_be_taken_once() {
        let signal = fake();
        let _first = signal.incoming();
        let second: Vec<_> = signal.incoming().collect().await;
        assert!(
            matches!(second.as_slice(), [Err(_)]),
            "second consumer got {second:?}"
        );
    }

    /// Pushed events must reach the consumer in order, or store tests built on
    /// the fake would pass against a reordering that real Signal never does.
    #[tokio::test]
    async fn pushed_events_arrive_in_order() {
        let signal = fake();
        let mut stream = signal.incoming();
        let target = MessageId {
            author: ServiceId::new(Uuid::from_u128(2)),
            sent_at_ms: 5,
        };
        let delete = Incoming::Delete {
            conversation: Recipient::Contact(target.author),
            target,
            from: target.author,
        };
        signal.push(delete.clone());
        signal.push(Incoming::Read {
            targets: vec![target],
        });
        let first = stream.next().await.expect("stream open").expect("ok");
        let second = stream.next().await.expect("stream open").expect("ok");
        assert_eq!(first, delete);
        assert!(
            matches!(second, Incoming::Read { .. }),
            "out of order: {second:?}"
        );
    }

    /// A rate-limited send must fail with the typed error callers match on,
    /// reach Signal exactly once, and leave the next send unaffected, so tool
    /// tests can tell "refused once" from "retried".
    #[tokio::test]
    async fn a_rate_limited_send_is_refused_once_and_counted_as_one_attempt() {
        let signal = fake();
        let limited = RateLimited {
            message: "Rate limit exceeded".into(),
            retry_after: Some(std::time::Duration::from_secs(30)),
        };
        signal.rate_limit_next_send(limited.clone());
        let to = Recipient::Contact(ServiceId::new(Uuid::from_u128(2)));
        let typing = Outgoing::Typing { started: true };
        let err = signal
            .send(to, typing.clone())
            .await
            .expect_err("a rate-limited send went through");
        assert_eq!(err.downcast_ref::<RateLimited>(), Some(&limited), "{err:#}");
        assert!(signal.sent().is_empty(), "refused send recorded as sent");
        signal.send(to, typing.clone()).await.expect("next send");
        assert_eq!(signal.attempts(), 2, "{:?}", signal.sent());
    }

    /// Sending to a group you're not in must fail, matching Signal.
    #[tokio::test]
    async fn sending_to_an_unknown_group_fails() {
        let signal = fake();
        let result = signal
            .send(
                Recipient::Group(GroupId::new([7; 32])),
                Outgoing::Typing { started: true },
            )
            .await;
        assert!(result.is_err(), "sent to a group we aren't in");
        assert!(signal.sent().is_empty(), "failed send was recorded");
    }
}
