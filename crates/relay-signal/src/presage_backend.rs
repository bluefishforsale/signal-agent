//! presage behind `SignalBackend` and `Provisioner`. presage's futures aren't
//! `Send` and its receive loop needs a `LocalSet`, so every call runs on one
//! dedicated thread and the trait methods only pass work to it.

use std::{
    collections::BTreeMap,
    future::Future,
    path::Path,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use presage::{
    Manager, manager::Registered, model::messages::Received, proto::GroupContextV2,
    store::ContentsStore,
};
use presage_store_sqlite::{OnNewIdentity, SqliteConnectOptions, SqliteStore};
use relay_core::prelude::*;
use tokio::sync::{mpsc, oneshot};

use crate::{Account, Contact, Group, Incoming, Outgoing, SignalBackend, convert};

type Job = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// The thread presage runs on. Dropping every handle stops it, and with it
/// every task still running there.
#[derive(Clone)]
struct Worker {
    jobs: mpsc::UnboundedSender<Job>,
}

impl Worker {
    fn spawn() -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("unable to start the Signal worker runtime")?;
        let (jobs, mut rx) = mpsc::unbounded_channel::<Job>();
        thread::Builder::new()
            .name("signal".into())
            .spawn(move || {
                let local = tokio::task::LocalSet::new();
                local.block_on(&runtime, async move {
                    while let Some(job) = rx.recv().await {
                        tokio::task::spawn_local(job());
                    }
                });
            })
            .context("unable to start the Signal worker thread")?;
        Ok(Self { jobs })
    }

    /// Runs `job` on the worker and waits for its result.
    async fn run<F, Fut, T>(&self, job: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Box::new(move || {
                Box::pin(async move {
                    // The caller gave up waiting; nobody wants the result.
                    let _ = tx.send(job().await);
                })
            }))
            .ok()
            .context("the Signal worker has stopped")?;
        rx.await.context("the Signal worker dropped a job")
    }
}

/// A registered account's connection to Signal through presage.
pub struct PresageSignal {
    worker: Worker,
    manager: Manager<SqliteStore, Registered>,
    account: Account,
    incoming_taken: AtomicBool,
}

impl PresageSignal {
    /// Opens the account in the store at `path`, encrypted with `key`. Nothing
    /// reaches Signal until something is sent or `incoming` is consumed.
    pub async fn open(path: &Path, key: &[u8; 32]) -> anyhow::Result<Self> {
        let worker = Worker::spawn()?;
        let (path, key) = (path.to_owned(), *key);
        let manager = worker
            .run(move || async move {
                let store = open_store(&path, &key).await?;
                Manager::load_registered(store).await.with_context(|| {
                    format!(
                        "no Signal account in \"{}\"; link or register first",
                        path.display()
                    )
                })
            })
            .await??;
        let account = convert::account(manager.registration_data(), manager.registration_type());
        Ok(Self {
            worker,
            manager,
            account,
            incoming_taken: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl SignalBackend for PresageSignal {
    async fn account(&self) -> anyhow::Result<Account> {
        Ok(self.account.clone())
    }

    async fn send(&self, to: Recipient, message: Outgoing) -> anyhow::Result<MessageId> {
        let me = self.account.service_id;
        if let Outgoing::Edit { target, .. } | Outgoing::Delete { target } = &message
            && target.author != me
        {
            bail!("Signal only edits and deletes your own messages; {target:?} is someone else's");
        }
        let sent_at_ms = now_ms()?;
        let manager = self.manager.clone();
        self.worker
            .run(move || send(manager, to, message, sent_at_ms))
            .await??;
        Ok(MessageId {
            author: me,
            sent_at_ms,
        })
    }

    async fn contacts(&self) -> anyhow::Result<Vec<Contact>> {
        let store = self.manager.store().clone();
        self.worker
            .run(move || async move {
                let mut contacts = Vec::new();
                for contact in store.contacts().await.context("unable to list contacts")? {
                    contacts.push(convert::contact_from(
                        &contact.context("unreadable contact in the store")?,
                    ));
                }
                anyhow::Ok(contacts)
            })
            .await?
    }

    async fn groups(&self) -> anyhow::Result<Vec<Group>> {
        let store = self.manager.store().clone();
        self.worker
            .run(move || async move {
                let mut groups = Vec::new();
                for group in store.groups().await.context("unable to list groups")? {
                    let (master_key, group) = group.context("unreadable group in the store")?;
                    groups.push(convert::group_from(master_key, &group));
                }
                anyhow::Ok(groups)
            })
            .await?
    }

    async fn attachment(&self, attachment: &AttachmentRef) -> anyhow::Result<Vec<u8>> {
        let pointer = convert::attachment_pointer(attachment)?;
        let manager = self.manager.clone();
        self.worker
            .run(move || async move { manager.get_attachment(&pointer).await })
            .await?
            .context("unable to download attachment")
    }

    fn incoming(&self) -> BoxStream<'static, anyhow::Result<Incoming>> {
        if self.incoming_taken.swap(true, Ordering::SeqCst) {
            return futures::stream::once(async {
                Err(anyhow!("incoming() already taken by another consumer"))
            })
            .boxed();
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (worker, manager, me) = (
            self.worker.clone(),
            self.manager.clone(),
            self.account.service_id,
        );
        // Connecting waits for the first poll, so holding the stream unpolled
        // never touches the network.
        let start = async move {
            let started = worker
                .run(move || async move {
                    tokio::task::spawn_local(receive(manager, me, tx));
                })
                .await;
            match started {
                Ok(()) => futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed(),
                Err(e) => futures::stream::once(async { Err(e) }).boxed(),
            }
        };
        futures::stream::once(start).flatten().boxed()
    }
}

async fn send(
    mut manager: Manager<SqliteStore, Registered>,
    to: Recipient,
    message: Outgoing,
    sent_at_ms: u64,
) -> anyhow::Result<()> {
    // Read receipts go to each message's author, wherever the messages were read.
    if let Outgoing::ReadReceipt { targets } = &message {
        let mut by_author: BTreeMap<ServiceId, Vec<MessageId>> = BTreeMap::new();
        for target in targets {
            by_author.entry(target.author).or_default().push(*target);
        }
        for (author, targets) in by_author {
            let body = convert::content_body(&Outgoing::ReadReceipt { targets }, sent_at_ms, None);
            manager
                .send_message(convert::aci(author), body, sent_at_ms)
                .await
                .with_context(|| format!("unable to send read receipts to {author}"))?;
        }
        return Ok(());
    }
    match to {
        Recipient::Contact(contact) => {
            let body = convert::content_body(&message, sent_at_ms, None);
            manager
                .send_message(convert::aci(contact), body, sent_at_ms)
                .await
                .with_context(|| format!("unable to send to {contact}"))
        }
        Recipient::Group(id) => {
            let (master_key, group) = group_by_id(manager.store(), id).await?;
            let context = GroupContextV2 {
                master_key: Some(master_key.to_vec()),
                revision: Some(group.revision),
                group_change: None,
            };
            let body = convert::content_body(&message, sent_at_ms, Some(&context));
            manager
                .send_message_to_group(&master_key, body, sent_at_ms)
                .await
                .with_context(|| format!("unable to send to group {id}"))
        }
    }
}

/// The cached group with public id `id`, with the master key sending needs.
async fn group_by_id(
    store: &SqliteStore,
    id: GroupId,
) -> anyhow::Result<([u8; 32], presage::model::groups::Group)> {
    for group in store.groups().await.context("unable to list groups")? {
        let (master_key, group) = group.context("unreadable group in the store")?;
        if convert::group_id(master_key) == id {
            return Ok((master_key, group));
        }
    }
    bail!("not a member of group {id}")
}

fn now_ms() -> anyhow::Result<u64> {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?;
    u64::try_from(since_epoch.as_millis()).context("system clock is past the year 500 million")
}

/// Receives from Signal for as long as anyone holds the other end of `tx`,
/// reconnecting with backoff whenever the connection drops.
async fn receive(
    mut manager: Manager<SqliteStore, Registered>,
    me: ServiceId,
    tx: mpsc::UnboundedSender<anyhow::Result<Incoming>>,
) {
    let mut delay = MIN_RECONNECT_DELAY;
    while !tx.is_closed() {
        match manager.receive_messages().await {
            Ok(messages) => {
                delay = MIN_RECONNECT_DELAY;
                let mut messages = std::pin::pin!(messages);
                while let Some(received) = messages.next().await {
                    let Received::Content(content) = received else {
                        continue;
                    };
                    let Some(event) = convert::incoming(&content, me) else {
                        continue;
                    };
                    if tx.send(Ok(event)).is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                let e = anyhow!(e).context("unable to receive from Signal");
                if tx.send(Err(e)).is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

const MIN_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(300);

/// Opens (creating if needed) presage's store at `path`, encrypted with `key`.
// NOTE: the key goes in as a raw SQLCipher key (x'..'), not a passphrase. It's
// already 32 random bytes, so PBKDF2 would only slow every open; but the form
// is part of the file format, and changing it locks out every existing store.
async fn open_store(path: &Path, key: &[u8; 32]) -> anyhow::Result<SqliteStore> {
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .pragma("key", format!("\"x'{hex}'\""));
    SqliteStore::open_with_options(options, OnNewIdentity::Trust)
        .await
        .with_context(|| format!("unable to open Signal store \"{}\"", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::{manager::RegistrationData, store::StateStore};

    fn temp_store() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("signal.db");
        (dir, path)
    }

    const KEY: [u8; 32] = [7; 32];
    const ME: &str = "00000000-0000-0000-0000-00000000003e";

    /// Writes a linked account's registration into a fresh store at `path`, as
    /// linking would, without touching Signal's servers.
    async fn register_linked(path: &Path) {
        let phone_number: presage::libsignal_service::prelude::phonenumber::PhoneNumber =
            "+15550000001".parse().expect("valid number");
        // presage's profile key deserializer borrows, so this goes through text.
        let json = serde_json::json!({
            "signal_servers": "Staging",
            "device_name": "Claude (test)",
            "phone_number": phone_number,
            "uuid": ME,
            "pni": "00000000-0000-0000-0000-0000000000ff",
            "password": "password",
            "device_id": 2,
            "registration_id": 1,
            "pni_registration_id": 1,
            "profile_key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        })
        .to_string();
        let data: RegistrationData = serde_json::from_str(&json).expect("registration json");
        seed(path, move |mut store| async move {
            store.save_registration_data(&data).await.expect("saved");
        })
        .await;
    }

    /// Runs `f` against the store at `path`, as presage's own receive loop
    /// would write to it.
    async fn seed<F, Fut>(path: &Path, f: F)
    where
        F: FnOnce(SqliteStore) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let path = path.to_owned();
        Worker::spawn()
            .expect("worker")
            .run(move || async move {
                let store = open_store(&path, &KEY).await.expect("store");
                f(store.clone()).await;
                store.close().await;
            })
            .await
            .expect("worker ran");
    }

    /// Contacts synced from the phone land in presage's store; the backend
    /// must list them from there, with no round trip to Signal.
    #[tokio::test]
    async fn contacts_synced_into_the_store_are_listed() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let alice = uuid::Uuid::from_u128(0xa11ce);
        seed(&path, move |mut store| async move {
            let contact = presage::model::contacts::Contact {
                uuid: alice,
                phone_number: None,
                name: "Alice".into(),
                verified: Default::default(),
                profile_key: Vec::new(),
                expire_timer: 0,
                expire_timer_version: 2,
                inbox_position: 0,
                avatar: None,
            };
            store.save_contact(&contact).await.expect("saved");
        })
        .await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        assert_eq!(
            signal.contacts().await.expect("contacts"),
            vec![Contact {
                service_id: ServiceId::new(alice),
                name: Some("Alice".into()),
                phone_number: None,
            }]
        );
    }

    /// The account comes from the store alone, so the agent can report who it
    /// is while Signal is unreachable.
    #[tokio::test]
    async fn account_is_read_from_the_store_without_the_network() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let account = signal.account().await.expect("account");
        assert_eq!(
            account,
            Account {
                service_id: ServiceId::parse(ME).expect("uuid"),
                phone_number: Some("+15550000001".into()),
                kind: ProfileKind::Linked,
                device_name: "Claude (test)".into(),
            }
        );
    }

    /// A group presage has cached, as it does on the first message from it.
    fn cached_group(title: &str, members: &[uuid::Uuid]) -> presage::model::groups::Group {
        presage::model::groups::Group {
            title: title.into(),
            avatar: String::new(),
            disappearing_messages_timer: None,
            access_control: None,
            revision: 1,
            members: members
                .iter()
                .map(|uuid| presage::model::groups::Member {
                    aci: presage::libsignal_service::protocol::Aci::from_uuid_bytes(
                        uuid.into_bytes(),
                    ),
                    role: presage::libsignal_service::groups_v2::Role::Default,
                    profile_key: presage::libsignal_service::prelude::ProfileKey::create([0; 32]),
                    joined_at_revision: 0,
                    label: None,
                    label_emoji: None,
                })
                .collect(),
            pending_members: Vec::new(),
            requesting_members: Vec::new(),
            invite_link_password: Vec::new(),
            description: None,
        }
    }

    /// Groups presage has cached are listed under their public id, straight
    /// from the store.
    #[tokio::test]
    async fn groups_cached_in_the_store_are_listed() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let me = uuid::Uuid::parse_str(ME).expect("uuid");
        seed(&path, move |store| async move {
            store
                .save_group([4; 32], cached_group("cats", &[me]))
                .await
                .expect("saved");
        })
        .await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let groups = signal.groups().await.expect("groups");
        assert_eq!(
            groups,
            vec![Group {
                id: convert::group_id([4; 32]),
                title: "cats".into(),
                members: vec![ServiceId::new(me)],
            }]
        );
    }

    /// The agent is the one consumer of incoming(); a second caller would steal
    /// half the messages, so it must get an error instead.
    #[tokio::test]
    async fn incoming_can_only_be_taken_once() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let _first = signal.incoming();
        let second: Vec<_> = signal.incoming().collect().await;
        assert!(
            matches!(second.as_slice(), [Err(_)]),
            "second consumer got {second:?}"
        );
    }

    /// An attachment id this backend never issued must fail with a clear error
    /// before anything reaches Signal.
    #[tokio::test]
    async fn unknown_attachment_id_fails_without_the_network() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let err = signal
            .attachment(&AttachmentRef {
                id: "not an id!".into(),
                content_type: String::new(),
                file_name: None,
                size: 0,
            })
            .await
            .expect_err("fetched a bogus attachment");
        assert!(
            err.to_string().contains("attachment id"),
            "unclear error: {err:#}"
        );
    }

    /// Signal only honors edits and deletes of your own messages; asking for
    /// someone else's must fail here, before a send the recipients would drop.
    #[tokio::test]
    async fn editing_or_deleting_someone_elses_message_is_refused() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let alice = ServiceId::new(uuid::Uuid::from_u128(0xa11ce));
        let theirs = MessageId {
            author: alice,
            sent_at_ms: 5,
        };
        for message in [
            Outgoing::Edit {
                target: theirs,
                body: "rewritten".into(),
            },
            Outgoing::Delete { target: theirs },
        ] {
            let err = signal
                .send(Recipient::Contact(alice), message.clone())
                .await
                .expect_err("sent an edit or delete of someone else's message");
            assert!(
                err.to_string().contains("your own"),
                "{message:?} failed for the wrong reason: {err:#}"
            );
        }
    }

    /// The store must be unreadable without its key. If the SQLCipher build
    /// ever falls back to plain SQLite the key pragma is silently ignored, and
    /// this is the test that notices.
    #[tokio::test]
    async fn store_refuses_to_open_with_the_wrong_key() {
        let (_dir, path) = temp_store();
        let worker = Worker::spawn().expect("worker");
        let open_path = path.clone();
        let store = worker
            .run(move || async move { open_store(&open_path, &[1; 32]).await })
            .await
            .expect("worker ran")
            .expect("first open creates the store");
        worker
            .run(move || async move { store.close().await })
            .await
            .expect("worker ran");
        let wrong = worker
            .run(move || async move { open_store(&path, &[2; 32]).await })
            .await
            .expect("worker ran");
        assert!(
            wrong.is_err(),
            "opened an encrypted store with the wrong key"
        );
    }
}
