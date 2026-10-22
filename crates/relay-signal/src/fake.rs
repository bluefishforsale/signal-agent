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

use crate::{
    Account, Contact, Device, Group, Identity, Incoming, Outgoing, SendableFile, SignalBackend,
};

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
    registered: HashMap<String, ServiceId>,
    usernames: HashMap<String, ServiceId>,
    avatars: HashMap<Recipient, Vec<u8>>,
    identities: Vec<Identity>,
    devices: Vec<Device>,
    sticker_packs: HashMap<StickerPackId, Vec<u32>>,
    uploads: u64,
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

    /// A phone number `discover` reports as registered to `service_id`.
    pub fn with_registered(self, number: impl Into<String>, service_id: ServiceId) -> Self {
        self.state().registered.insert(number.into(), service_id);
        self
    }

    pub fn with_username(self, username: impl Into<String>, service_id: ServiceId) -> Self {
        self.state().usernames.insert(username.into(), service_id);
        self
    }

    pub fn with_avatar(self, of: Recipient, bytes: Vec<u8>) -> Self {
        self.state().avatars.insert(of, bytes);
        self
    }

    pub fn with_identities(self, identities: Vec<Identity>) -> Self {
        self.state().identities = identities;
        self
    }

    pub fn with_devices(self, devices: Vec<Device>) -> Self {
        self.state().devices = devices;
        self
    }

    /// An installed sticker pack holding `sticker_ids`.
    pub fn with_sticker_pack(self, pack: StickerPackId, sticker_ids: Vec<u32>) -> Self {
        self.state().sticker_packs.insert(pack, sticker_ids);
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

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("fake signal state")
    }

    /// Fails the sends the real backend can't make, so tool tests can't pass
    /// on a message that would never leave the machine.
    fn refuse_what_signal_would(&self, to: Recipient, message: &Outgoing) -> anyhow::Result<()> {
        let state = self.state();
        match message {
            Outgoing::Text { attachments, .. } => {
                if let Some(unknown) = attachments
                    .iter()
                    .find(|a| !state.attachments.contains_key(&a.id))
                {
                    bail!("attachment \"{}\" was never uploaded", unknown.id);
                }
            }
            Outgoing::Sticker { pack, sticker_id } => {
                let Some(stickers) = state.sticker_packs.get(pack) else {
                    bail!("sticker pack {pack} is not installed");
                };
                if !stickers.contains(sticker_id) {
                    bail!("sticker pack {pack} has no sticker {sticker_id}");
                }
            }
            Outgoing::AdminDelete { .. } if matches!(to, Recipient::Contact(_)) => {
                bail!("an admin delete only goes to a group");
            }
            // The wire form names only the poll's timestamp, so it can only
            // ever close one of this account's own polls.
            Outgoing::PollTerminate { poll } if poll.author != self.account.service_id => {
                bail!("only the poll's creator can end it; {poll:?} is someone else's");
            }
            _ => {}
        }
        Ok(())
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
        self.refuse_what_signal_would(to, &message)?;
        self.state().sent.push((to, message));
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

    async fn upload(&self, file: &SendableFile) -> anyhow::Result<AttachmentRef> {
        let bytes = std::fs::read(file.path())
            .with_context(|| format!("unable to read \"{}\"", file.path().display()))?;
        let mut state = self.state();
        state.uploads += 1;
        let attachment = AttachmentRef {
            id: format!("upload-{}", state.uploads),
            content_type: "application/octet-stream".into(),
            file_name: file
                .path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            size: bytes.len() as u64,
        };
        state.attachments.insert(attachment.id.clone(), bytes);
        Ok(attachment)
    }

    async fn discover(
        &self,
        numbers: &[String],
    ) -> anyhow::Result<Vec<(String, Option<ServiceId>)>> {
        let state = self.state();
        Ok(numbers
            .iter()
            .map(|n| (n.clone(), state.registered.get(n).copied()))
            .collect())
    }

    async fn lookup_username(&self, username: &str) -> anyhow::Result<Option<ServiceId>> {
        Ok(self.state().usernames.get(username).copied())
    }

    async fn avatar(&self, of: Recipient) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.state().avatars.get(&of).cloned())
    }

    async fn identities(&self) -> anyhow::Result<Vec<Identity>> {
        Ok(self.state().identities.clone())
    }

    async fn devices(&self) -> anyhow::Result<Vec<Device>> {
        Ok(self.state().devices.clone())
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

    fn mid(author: u128, sent_at_ms: u64) -> MessageId {
        MessageId {
            author: ServiceId::new(Uuid::from_u128(author)),
            sent_at_ms,
        }
    }

    fn text_with(attachments: Vec<AttachmentRef>) -> Outgoing {
        Outgoing::Text {
            body: "see attached".into(),
            quote: None,
            attachments,
        }
    }

    /// An uploaded file must be sendable by the ref upload returned and read
    /// back with the same bytes, or the send-attachment tools would store refs
    /// that get_attachment can't fetch.
    #[tokio::test]
    async fn an_uploaded_file_is_sendable_and_reads_back_by_its_ref() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("cat.png"), b"png bytes").expect("file");
        let roots = crate::SendRoots::new([dir.path().to_path_buf()]).expect("roots");
        let file = roots.admit(&dir.path().join("cat.png")).expect("admit");
        let signal = fake();
        let uploaded = signal.upload(&file).await.expect("upload");
        assert_eq!(uploaded.file_name.as_deref(), Some("cat.png"));
        assert_eq!(uploaded.size, 9);
        let to = Recipient::Contact(ServiceId::new(Uuid::from_u128(2)));
        signal
            .send(to, text_with(vec![uploaded.clone()]))
            .await
            .expect("send with the upload");
        let bytes = signal.attachment(&uploaded).await.expect("read back");
        assert_eq!(bytes, b"png bytes");
    }

    /// Text can only carry attachments this backend issued; a made-up ref
    /// would fail in the real backend's pointer decoding.
    #[tokio::test]
    async fn text_with_an_attachment_never_uploaded_fails() {
        let signal = fake();
        let to = Recipient::Contact(ServiceId::new(Uuid::from_u128(2)));
        let forged = AttachmentRef {
            id: "made-up".into(),
            content_type: "image/png".into(),
            file_name: None,
            size: 1,
        };
        let result = signal.send(to, text_with(vec![forged])).await;
        assert!(result.is_err(), "sent a forged attachment");
        assert!(signal.sent().is_empty(), "failed send was recorded");
    }

    /// A sticker needs its pack installed and the sticker in it, as the real
    /// backend looks the pack key up in the store.
    #[tokio::test]
    async fn a_sticker_outside_the_installed_packs_fails() {
        let pack = StickerPackId::new([1; 16]);
        let signal = fake().with_sticker_pack(pack, vec![0, 1]);
        let to = Recipient::Contact(ServiceId::new(Uuid::from_u128(2)));
        let sticker = |pack, sticker_id| Outgoing::Sticker { pack, sticker_id };
        signal.send(to, sticker(pack, 1)).await.expect("installed");
        for bad in [sticker(pack, 9), sticker(StickerPackId::new([2; 16]), 0)] {
            let result = signal.send(to, bad.clone()).await;
            assert!(result.is_err(), "sent {bad:?}");
        }
        assert_eq!(signal.sent().len(), 1);
    }

    /// An admin delete is a group act, and a poll can only be ended by its
    /// creator; the fake must refuse both misuses as Signal would.
    #[tokio::test]
    async fn admin_deletes_to_a_person_and_ending_someone_elses_poll_fail() {
        let signal = fake();
        let alice = Recipient::Contact(ServiceId::new(Uuid::from_u128(2)));
        for bad in [
            Outgoing::AdminDelete { target: mid(2, 5) },
            Outgoing::PollTerminate { poll: mid(2, 5) },
        ] {
            let result = signal.send(alice, bad.clone()).await;
            assert!(result.is_err(), "sent {bad:?}");
        }
        signal
            .send(alice, Outgoing::PollTerminate { poll: mid(1, 5) })
            .await
            .expect("own poll");
        assert_eq!(signal.sent().len(), 1);
    }

    /// Discovery answers every number asked, in order, with None for the
    /// unregistered, so a tool can zip answers back onto its input.
    #[tokio::test]
    async fn discover_answers_every_number_in_the_order_asked() {
        let bob = ServiceId::new(Uuid::from_u128(3));
        let signal = fake().with_registered("+15550000003", bob);
        let asked = ["+15550000009".to_owned(), "+15550000003".to_owned()];
        let got = signal.discover(&asked).await.expect("discover");
        assert_eq!(
            got,
            vec![
                ("+15550000009".to_owned(), None),
                ("+15550000003".to_owned(), Some(bob)),
            ]
        );
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
