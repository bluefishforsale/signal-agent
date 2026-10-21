//! presage behind `SignalBackend` and `Provisioner`. presage's futures aren't
//! `Send` and its receive loop needs a `LocalSet`, so every call runs on one
//! dedicated thread and the trait methods only pass work to it.

use std::{future::Future, path::Path, pin::Pin, thread};

use anyhow::{Context, bail};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use presage::{Manager, manager::Registered, store::ContentsStore};
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
        })
    }
}

#[async_trait]
impl SignalBackend for PresageSignal {
    async fn account(&self) -> anyhow::Result<Account> {
        Ok(self.account.clone())
    }

    async fn send(&self, to: Recipient, message: Outgoing) -> anyhow::Result<MessageId> {
        bail!("not yet")
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
        bail!("not yet")
    }

    async fn attachment(&self, attachment: &AttachmentRef) -> anyhow::Result<Vec<u8>> {
        bail!("not yet")
    }

    fn incoming(&self) -> BoxStream<'static, anyhow::Result<Incoming>> {
        futures::stream::empty().boxed()
    }
}

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
