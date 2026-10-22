//! presage behind `SignalBackend` and `Provisioner`. presage's futures aren't
//! `Send` and its receive loop needs a `LocalSet`, so every call runs on one
//! dedicated thread and the trait methods only pass work to it.

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Display,
    future::Future,
    path::Path,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use presage::{
    Manager,
    libsignal_service::{
        configuration::SignalServers,
        prelude::{MessageSenderError, ServiceError, phonenumber::PhoneNumber},
        provisioning::ProvisioningError,
    },
    libsignal_service::{
        content::{Content, ContentBody},
        protocol::{
            DeviceId, IdentityKey, IdentityKeyStore, ProtocolAddress, ServiceId as SignalServiceId,
        },
        sender::{AttachmentSpec, AttachmentUploadError},
    },
    manager::{Confirmation, Registered, RegistrationOptions},
    model::messages::Received,
    proto::{AttachmentPointer, GroupContextV2},
    store::{ContentsStore, StateStore, Store, Thread},
};
use presage_store_sqlite::{OnNewIdentity, SqliteConnectOptions, SqliteStore};
use relay_core::prelude::*;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::{
    Account, Contact, Device, Group, Identity, Incoming, LinkEvent, Outgoing, Provisioner,
    RateLimited, RegistrationStep, SendableFile, SignalBackend, convert,
};

type Job = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// The thread presage runs on, with the store it owns. Dropping the last
/// handle stops the thread's tasks, closes the store and waits for both.
#[derive(Clone)]
struct Worker {
    inner: Arc<WorkerInner>,
}

struct WorkerInner {
    jobs: Option<mpsc::UnboundedSender<Job>>,
    thread: Option<thread::JoinHandle<()>>,
    store: SqliteStore,
}

// NOTE: a SQLCipher pool dropped without close() can crash at process exit in
// SQLCipher's codec cleanup, so the drop blocks until the worker has closed the
// store. Never drop the last handle on the worker thread itself.
impl Drop for WorkerInner {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(handle) = self.thread.take()
            && handle.thread().id() != thread::current().id()
        {
            // A panicked worker has nothing left to close.
            let _ = handle.join();
        }
    }
}

impl Worker {
    /// Starts the worker on the store at `path`, encrypted with `key`.
    async fn open(path: &Path, key: &[u8; 32]) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("unable to start the Signal worker runtime")?;
        let (jobs, mut rx) = mpsc::unbounded_channel::<Job>();
        let (opened_tx, opened_rx) = oneshot::channel();
        let (path, key) = (path.to_owned(), *key);
        let thread = thread::Builder::new()
            .name("signal".into())
            .spawn(move || {
                let local = tokio::task::LocalSet::new();
                let store = local.block_on(&runtime, async move {
                    let store = match open_store(&path, &key).await {
                        Ok(store) => store,
                        Err(e) => {
                            let _ = opened_tx.send(Err(e));
                            return None;
                        }
                    };
                    let _ = opened_tx.send(Ok(store.clone()));
                    while let Some(job) = rx.recv().await {
                        tokio::task::spawn_local(job());
                    }
                    Some(store)
                });
                // Stop every task still holding the store before closing it.
                drop(local);
                if let Some(store) = store {
                    runtime.block_on(store.close());
                }
            })
            .context("unable to start the Signal worker thread")?;
        let store = opened_rx
            .await
            .context("the Signal worker stopped while opening the store")??;
        Ok(Self {
            inner: Arc::new(WorkerInner {
                jobs: Some(jobs),
                thread: Some(thread),
                store,
            }),
        })
    }

    fn store(&self) -> SqliteStore {
        self.inner.store.clone()
    }

    /// Runs `job` on the worker and waits for its result.
    async fn run<F, Fut, T>(&self, job: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.inner
            .jobs
            .as_ref()
            .context("the Signal worker has stopped")?
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
    last_sent_at_ms: AtomicU64,
}

impl PresageSignal {
    /// Opens the account in the store at `path`, encrypted with `key`. Nothing
    /// reaches Signal until something is sent or `incoming` is consumed.
    pub async fn open(path: &Path, key: &[u8; 32]) -> anyhow::Result<Self> {
        let worker = Worker::open(path, key).await?;
        let (path, store) = (path.to_owned(), worker.store());
        let manager = worker
            .run(move || async move {
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
            last_sent_at_ms: AtomicU64::new(0),
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
        if let Outgoing::PollTerminate { poll } = &message
            && poll.author != me
        {
            bail!("Signal only ends your own polls; {poll:?} is someone else's");
        }
        let sent_at_ms = next_timestamp(&self.last_sent_at_ms, now_ms()?);
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
            .map_err(|e| signal_error(e, "unable to download attachment"))
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
    async fn upload(&self, file: &SendableFile) -> anyhow::Result<AttachmentRef> {
        let path = file.path().to_owned();
        let size = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("unable to read \"{}\"", path.display()))?
            .len();
        if size > MAX_ATTACHMENT_BYTES {
            bail!(
                "\"{}\" is {size} bytes, larger than the {MAX_ATTACHMENT_BYTES} Signal accepts",
                path.display()
            );
        }
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("unable to read \"{}\"", path.display()))?;
        let spec = convert::attachment_spec(&path, bytes.len());
        let manager = self.manager.clone();
        let pointer = self
            .worker
            .run(move || async move { upload(&manager, spec, bytes).await })
            .await??;
        Ok(convert::attachment_ref(&pointer))
    }

    async fn discover(
        &self,
        _numbers: &[String],
    ) -> anyhow::Result<Vec<(String, Option<ServiceId>)>> {
        bail!("contact discovery is not implemented yet (lane L1b)")
    }

    async fn lookup_username(&self, _username: &str) -> anyhow::Result<Option<ServiceId>> {
        bail!("username lookup is not implemented yet (lane L1b)")
    }

    async fn avatar(&self, _of: Recipient) -> anyhow::Result<Option<Vec<u8>>> {
        bail!("avatars are not implemented yet (lane L1b)")
    }

    async fn identities(&self) -> anyhow::Result<Vec<Identity>> {
        bail!("listing identities is not implemented yet (lane L1b)")
    }

    async fn devices(&self) -> anyhow::Result<Vec<Device>> {
        bail!("listing devices is not implemented yet (lane L1b)")
    }
}

/// The largest attachment Signal's apps accept.
const MAX_ATTACHMENT_BYTES: u64 = 100 * 1024 * 1024;

/// Which Signal service an account lives on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Servers {
    Production,
    /// Signal's staging service, for the nightly two-account test.
    Staging,
}

/// Creates the account in one profile's presage store.
pub struct PresageProvisioner {
    worker: Worker,
    store: SqliteStore,
    servers: SignalServers,
    pending: Mutex<Option<Manager<SqliteStore, Confirmation>>>,
}

impl PresageProvisioner {
    /// Opens (creating if needed) the store at `path`, encrypted with `key`,
    /// for an account on `servers`.
    pub async fn open(path: &Path, key: &[u8; 32], servers: Servers) -> anyhow::Result<Self> {
        let worker = Worker::open(path, key).await?;
        Ok(Self {
            store: worker.store(),
            worker,
            servers: match servers {
                Servers::Production => SignalServers::Production,
                Servers::Staging => SignalServers::Staging,
            },
            pending: Mutex::new(None),
        })
    }
}

#[async_trait]
impl Provisioner for PresageProvisioner {
    fn link(&self, device_name: String) -> BoxStream<'static, anyhow::Result<LinkEvent>> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (worker, store, servers) = (self.worker.clone(), self.store.clone(), self.servers);
        let start = async move {
            let started = worker
                .run(move || async move {
                    tokio::task::spawn_local(link(store, servers, device_name, tx));
                })
                .await;
            match started {
                Ok(()) => futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed(),
                Err(e) => futures::stream::once(async { Err(e) }).boxed(),
            }
        };
        futures::stream::once(start).flatten().boxed()
    }

    async fn request_code(
        &self,
        phone_number: &str,
        captcha: Option<&str>,
        voice: bool,
    ) -> anyhow::Result<RegistrationStep> {
        let phone_number: PhoneNumber = phone_number
            .parse()
            .with_context(|| format!("\"{phone_number}\" is not a phone number"))?;
        let captcha = captcha.map(str::to_owned);
        let (store, servers) = (self.store.clone(), self.servers);
        let requested = self
            .worker
            .run(move || async move {
                Manager::register(
                    store,
                    RegistrationOptions {
                        signal_servers: servers,
                        phone_number,
                        use_voice_call: voice,
                        captcha: captcha.as_deref(),
                        force: false,
                    },
                )
                .await
            })
            .await?;
        match requested {
            Ok(confirmation) => {
                *self.pending.lock().expect("pending registration lock") = Some(confirmation);
                Ok(RegistrationStep::CodeSent)
            }
            Err(presage::Error::CaptchaRequired) => Ok(RegistrationStep::CaptchaRequired),
            Err(e) => Err(signal_error(e, "unable to request a verification code")),
        }
    }

    async fn verify_code(&self, code: &str) -> anyhow::Result<RegistrationStep> {
        // Cloned, not taken: a mistyped code can be retried on the same session.
        let Some(confirmation) = self
            .pending
            .lock()
            .expect("pending registration lock")
            .clone()
        else {
            bail!("no verification code is pending; request a code first");
        };
        let code = code.trim().to_owned();
        let manager = self
            .worker
            .run(move || async move { confirmation.confirm_verification_code(code).await })
            .await?
            .map_err(|e| signal_error(e, "Signal did not accept the verification code"))?;
        self.pending
            .lock()
            .expect("pending registration lock")
            .take();
        Ok(RegistrationStep::Registered {
            account: convert::account(manager.registration_data(), manager.registration_type()),
        })
    }
}

/// Links as a secondary device, offering a fresh URI each time an unscanned
/// one expires, until the phone accepts or the consumer goes away.
async fn link(
    store: SqliteStore,
    servers: SignalServers,
    device_name: String,
    tx: mpsc::UnboundedSender<anyhow::Result<LinkEvent>>,
) {
    if store.is_registered().await {
        // The consumer may already be gone; nothing else to tell.
        let _ = tx.send(Err(anyhow!(
            "this profile already has a Signal account; linking would erase it"
        )));
        return;
    }
    while !tx.is_closed() {
        let (uri_tx, uri_rx) = futures::channel::oneshot::channel::<url::Url>();
        let show_uri = {
            let tx = tx.clone();
            async move {
                if let Ok(uri) = uri_rx.await {
                    let _ = tx.send(Ok(LinkEvent::Uri {
                        uri: uri.to_string(),
                    }));
                }
            }
        };
        let (linked, ()) = futures::future::join(
            Manager::link_secondary_device(store.clone(), servers, device_name.clone(), uri_tx),
            show_uri,
        )
        .await;
        let event = match linked {
            Ok(manager) => Ok(LinkEvent::Linked {
                account: convert::account(manager.registration_data(), manager.registration_type()),
            }),
            // The provisioning socket closed with no scan: the URI expired.
            // NOTE: only this loops. Any other failure, a rate limit above all,
            // ends the link; looping on it would re-ask Signal unattended.
            Err(presage::Error::ProvisioningError(ProvisioningError::MissingMessage)) => continue,
            Err(e) => Err(signal_error(e, "linking failed")),
        };
        let _ = tx.send(event);
        return;
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
            let body = convert::content_body(&Outgoing::ReadReceipt { targets }, sent_at_ms, None)?;
            manager
                .send_message(convert::aci(author), body, sent_at_ms)
                .await
                .map_err(|e| {
                    signal_error(e, format!("unable to send read receipts to {author}"))
                })?;
        }
        return Ok(());
    }
    match to {
        Recipient::Contact(contact) => {
            let body = content_body(&manager, &message, sent_at_ms, None).await?;
            manager
                .send_message(convert::aci(contact), body, sent_at_ms)
                .await
                .map_err(|e| signal_error(e, format!("unable to send to {contact}")))
        }
        Recipient::Group(id) => {
            let (master_key, group) = group_by_id(manager.store(), id).await?;
            let context = GroupContextV2 {
                master_key: Some(master_key.to_vec()),
                revision: Some(group.revision),
                group_change: None,
            };
            let body = content_body(&manager, &message, sent_at_ms, Some(&context)).await?;
            manager
                .send_message_to_group(&master_key, body, sent_at_ms)
                .await
                .map_err(|e| signal_error(e, format!("unable to send to group {id}")))
        }
    }
}

/// What to put on the wire for `message`, uploading a sticker's image first.
// NOTE: the sticker is looked up before anything is uploaded, so a pack that
// isn't installed fails without reaching Signal.
async fn content_body(
    manager: &Manager<SqliteStore, Registered>,
    message: &Outgoing,
    sent_at_ms: u64,
    group: Option<&GroupContextV2>,
) -> anyhow::Result<ContentBody> {
    let Outgoing::Sticker { pack, sticker_id } = message else {
        return convert::content_body(message, sent_at_ms, group);
    };
    let Some(installed) = manager
        .store()
        .sticker_pack(pack.as_bytes())
        .await
        .context("unable to read sticker packs")?
    else {
        bail!("sticker pack {pack} is not installed");
    };
    let Some(sticker) = installed
        .manifest
        .stickers
        .iter()
        .find(|s| s.id == *sticker_id)
    else {
        bail!("sticker pack {pack} has no sticker {sticker_id}");
    };
    let Some(bytes) = sticker.bytes.clone() else {
        bail!("sticker {sticker_id} of pack {pack} was never downloaded");
    };
    let spec = AttachmentSpec {
        content_type: sticker
            .content_type
            .clone()
            .unwrap_or_else(|| "image/webp".into()),
        length: bytes.len(),
        ..Default::default()
    };
    let image = upload(manager, spec, bytes).await?;
    Ok(convert::sticker_body(
        *pack,
        &installed.key,
        *sticker_id,
        sticker.emoji.as_deref(),
        image,
        sent_at_ms,
        group,
    ))
}

/// Encrypts and uploads `bytes`, returning the pointer a message carries.
async fn upload(
    manager: &Manager<SqliteStore, Registered>,
    spec: AttachmentSpec,
    bytes: Vec<u8>,
) -> anyhow::Result<AttachmentPointer> {
    manager
        .upload_attachment(spec, bytes)
        .await
        .map_err(|e| signal_error(e, "unable to upload attachment"))?
        .map_err(upload_error)
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

/// A send timestamp: the clock, or one past the last one when the clock
/// hasn't moved on.
fn next_timestamp(last: &AtomicU64, now_ms: u64) -> u64 {
    let previous = last
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| {
            Some(now_ms.max(last + 1))
        })
        .unwrap_or_else(|last| last);
    now_ms.max(previous + 1)
}

fn now_ms() -> anyhow::Result<u64> {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?;
    u64::try_from(since_epoch.as_millis()).context("system clock is past the year 500 million")
}

/// Whether `content` may be kept: anything outside a group, or group traffic
/// from a current member.
// NOTE: presage refreshes a group from the server before yielding content when
// the group is uncached or the message claims a newer revision, so the cache
// checked here is already as fresh as the sender can force it to be. presage
// has no public call to force a fetch otherwise.
async fn admitted(store: &SqliteStore, content: &Content) -> bool {
    let Ok(Thread::Group(master_key)) = Thread::try_from(content) else {
        return true;
    };
    let sender = content.metadata.sender;
    let group = match store.group(master_key).await {
        Ok(Some(group)) => group,
        Ok(None) => {
            warn!(group = %convert::group_id(master_key), "dropping traffic for an unknown group");
            return false;
        }
        Err(error) => {
            warn!(%error, "dropping group traffic: unable to read the group");
            return false;
        }
    };
    let is_member = group
        .members
        .iter()
        .any(|m| SignalServiceId::from(m.aci) == sender);
    if !is_member {
        warn!(
            group = %convert::group_id(master_key),
            sender = %sender.service_id_string(),
            "dropping group traffic from a non-member"
        );
    }
    is_member
}

/// The last identity key seen for each contact. The store trusts a changed key
/// without saying so; comparing against this is how the change still surfaces.
// NOTE: primed from contacts and group members when receiving starts, so a key
// that changed while the agent was down is caught on that contact's next
// message. A stranger's first key is new, not changed, and isn't reported.
struct IdentityWatch {
    known: HashMap<String, IdentityKey>,
}

impl IdentityWatch {
    async fn primed(store: &SqliteStore) -> Self {
        let mut people: Vec<SignalServiceId> = Vec::new();
        match store.contacts().await {
            Ok(contacts) => people.extend(
                contacts
                    .flatten()
                    .map(|c| SignalServiceId::from(convert::aci(ServiceId::new(c.uuid)))),
            ),
            Err(error) => warn!(%error, "identity watch starts without contacts"),
        }
        match store.groups().await {
            Ok(groups) => {
                for (_, group) in groups.flatten() {
                    people.extend(group.members.iter().map(|m| SignalServiceId::from(m.aci)));
                }
            }
            Err(error) => warn!(%error, "identity watch starts without group members"),
        }
        let mut watch = Self {
            known: HashMap::new(),
        };
        for who in people {
            watch.changed(store, who).await;
        }
        watch
    }

    /// Whether `who`'s identity key differs from the last one seen.
    async fn changed(&mut self, store: &SqliteStore, who: SignalServiceId) -> bool {
        let name = who.service_id_string();
        // Identity keys are per account; the device id is ignored by the lookup.
        let Ok(device) = DeviceId::new(1) else {
            return false;
        };
        let address = ProtocolAddress::new(name.clone(), device);
        let current = match store.aci_protocol_store().get_identity(&address).await {
            Ok(Some(current)) => current,
            Ok(None) => return false,
            Err(error) => {
                warn!(%error, contact = %name, "unable to read an identity key");
                return false;
            }
        };
        self.known
            .insert(name, current)
            .is_some_and(|previous| previous != current)
    }
}

/// Receives from Signal for as long as anyone holds the other end of `tx`,
/// reconnecting with backoff whenever the connection drops.
// NOTE: reconnecting only reopens the receive socket; nothing a caller asked to
// send is ever resent from here.
async fn receive(
    mut manager: Manager<SqliteStore, Registered>,
    me: ServiceId,
    tx: mpsc::UnboundedSender<anyhow::Result<Incoming>>,
) {
    let mut identities = IdentityWatch::primed(manager.store()).await;
    let mut delay = MIN_RECONNECT_DELAY;
    while !tx.is_closed() {
        let mut wait = delay;
        match manager.receive_messages().await {
            Ok(messages) => {
                delay = MIN_RECONNECT_DELAY;
                let mut messages = std::pin::pin!(messages);
                while let Some(received) = messages.next().await {
                    let Received::Content(content) = received else {
                        continue;
                    };
                    let sender = content.metadata.sender;
                    if identities.changed(manager.store(), sender).await {
                        let contact = ServiceId::new(sender.raw_uuid());
                        if tx.send(Ok(Incoming::IdentityChanged { contact })).is_err() {
                            return;
                        }
                    }
                    if !admitted(manager.store(), &content).await {
                        continue;
                    }
                    let Some(event) = convert::incoming(&content, me) else {
                        continue;
                    };
                    if tx.send(Ok(event)).is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                wait = reconnect_wait(delay, rate_limited(&e).as_ref());
                let e = signal_error(e, "unable to receive from Signal");
                if tx.send(Err(e)).is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(wait).await;
        delay = (delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

const MIN_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(300);

/// How long to wait before reconnecting: the backoff `delay`, unless Signal
/// refused the connection as rate-limited.
fn reconnect_wait(delay: Duration, limited: Option<&RateLimited>) -> Duration {
    let Some(limited) = limited else {
        return delay;
    };
    limited
        .retry_after
        .unwrap_or_default()
        .max(MAX_RECONNECT_DELAY)
}

/// `error` as the typed `RateLimited` when Signal rate-limited the request,
/// so callers can match on it and show its wait; otherwise under `context`.
fn signal_error<S>(
    error: presage::Error<S>,
    context: impl Display + Send + Sync + 'static,
) -> anyhow::Error
where
    S: std::error::Error + Send + Sync + 'static,
{
    match rate_limited(&error) {
        Some(limited) => limited.into(),
        None => anyhow!(error).context(context),
    }
}

/// The rate limit or proof request behind `error`, if Signal refused with one.
fn rate_limited<S: std::error::Error>(error: &presage::Error<S>) -> Option<RateLimited> {
    match error {
        presage::Error::ServiceError(service)
        | presage::Error::ProvisioningError(ProvisioningError::ServiceError(service)) => {
            service_rate_limited(service)
        }
        presage::Error::MessageSenderError(sender) => match sender.as_ref() {
            MessageSenderError::ServiceError(service) => service_rate_limited(service),
            MessageSenderError::ProofRequired { options, .. } => Some(proof_required(options)),
            _ => None,
        },
        _ => None,
    }
}

fn service_rate_limited(error: &ServiceError) -> Option<RateLimited> {
    match error {
        ServiceError::RateLimitExceeded { retry_after } => Some(RateLimited {
            message: error.to_string(),
            retry_after: retry_after.and_then(|wait| wait.to_std().ok()),
        }),
        ServiceError::ProofRequiredError(proof) => Some(proof_required(&proof.options)),
        ServiceError::WsError(ws) => match ws.as_ref() {
            reqwest_websocket::Error::Handshake(
                reqwest_websocket::HandshakeError::UnexpectedStatusCode(status),
            ) if status.as_u16() == 429 => Some(RateLimited {
                message: format!("websocket connection refused: {status}"),
                retry_after: None,
            }),
            _ => None,
        },
        // The session's Debug form carries its id; only the waits are Signal's
        // message here.
        ServiceError::VerificationSessionRateLimited {
            session,
            retry_after,
        } => {
            let waits = [
                ("next SMS", session.next_sms),
                ("next call", session.next_call),
                ("next code check", session.next_verification_attempt),
            ];
            let mut message = String::from("verification session rate limited");
            for (what, seconds) in waits {
                if let Some(seconds) = seconds {
                    message.push_str(&format!("; {what} in {seconds}s"));
                }
            }
            Some(RateLimited {
                message,
                retry_after: retry_after.and_then(|wait| wait.to_std().ok()),
            })
        }
        _ => None,
    }
}

/// A failed upload, as the typed `RateLimited` when Signal rate-limited it.
fn upload_error(error: AttachmentUploadError) -> anyhow::Error {
    if let AttachmentUploadError::ServiceError(service) = &error
        && let Some(limited) = service_rate_limited(service)
    {
        return limited.into();
    }
    anyhow!(error).context("unable to upload attachment")
}

// NOTE: the challenge token stays out of the message; it would otherwise reach
// whatever MCP client asked for the send.
fn proof_required(options: &[String]) -> RateLimited {
    RateLimited {
        message: format!("proof required: {}", options.join(", ")),
        retry_after: None,
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
    use presage::{
        libsignal_service::{
            content::{ContentBody, Metadata},
            websocket::registration::RegistrationSessionMetadataResponse,
        },
        manager::RegistrationData,
    };

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
        let worker = Worker::open(path, &KEY).await.expect("worker");
        let store = worker.store();
        worker.run(move || f(store)).await.expect("worker ran");
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
                admins: vec![],
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

    /// Ending a poll names only its timestamp, which receivers pair with the
    /// sender; ending someone else's poll would close one of ours, or nothing,
    /// so it must fail here before reaching Signal.
    #[tokio::test]
    async fn ending_someone_elses_poll_is_refused() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let alice = ServiceId::new(uuid::Uuid::from_u128(0xa11ce));
        let theirs = MessageId {
            author: alice,
            sent_at_ms: 5,
        };
        let err = signal
            .send(
                Recipient::Contact(alice),
                Outgoing::PollTerminate { poll: theirs },
            )
            .await
            .expect_err("ended someone else's poll");
        assert!(
            err.to_string().contains("your own"),
            "failed for the wrong reason: {err:#}"
        );
    }

    /// A sticker's pack key comes from presage's store; a pack the account
    /// never installed has none, so the send must fail before reaching Signal.
    #[tokio::test]
    async fn a_sticker_from_a_pack_not_installed_fails_without_the_network() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let alice = ServiceId::new(uuid::Uuid::from_u128(0xa11ce));
        let err = signal
            .send(
                Recipient::Contact(alice),
                Outgoing::Sticker {
                    pack: StickerPackId::new([1; 16]),
                    sticker_id: 0,
                },
            )
            .await
            .expect_err("sent a sticker from a pack never installed");
        assert!(
            err.to_string().contains("not installed"),
            "unclear error: {err:#}"
        );
    }

    /// An installed pack without the asked-for sticker, or whose image never
    /// downloaded, has nothing to send; both must fail before any upload.
    #[tokio::test]
    async fn a_sticker_missing_from_its_installed_pack_fails_without_the_network() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let pack = StickerPackId::new([1; 16]);
        seed(&path, move |mut store| async move {
            let sticker = |id, bytes| presage::store::Sticker {
                id,
                emoji: None,
                content_type: Some("image/webp".into()),
                bytes,
            };
            store
                .add_sticker_pack(&presage::store::StickerPack {
                    id: pack.as_bytes().to_vec(),
                    key: vec![2; 32],
                    manifest: presage::store::StickerPackManifest {
                        title: "cats".into(),
                        author: "me".into(),
                        cover: None,
                        stickers: vec![sticker(0, None), sticker(1, Some(vec![1, 2, 3]))],
                    },
                })
                .await
                .expect("saved");
        })
        .await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let alice = Recipient::Contact(ServiceId::new(uuid::Uuid::from_u128(0xa11ce)));
        for (sticker_id, reason) in [(9, "has no sticker"), (0, "never downloaded")] {
            let err = signal
                .send(alice, Outgoing::Sticker { pack, sticker_id })
                .await
                .expect_err("sent a sticker with no image");
            assert!(
                err.to_string().contains(reason),
                "sticker {sticker_id}: unclear error: {err:#}"
            );
        }
    }

    /// A file past Signal's attachment limit would be read whole into memory
    /// only for Signal to refuse it; it must fail before either happens.
    #[tokio::test]
    async fn a_file_over_signals_attachment_limit_is_refused_before_reading() {
        let (dir, path) = temp_store();
        register_linked(&path).await;
        let big = dir.path().join("big.bin");
        std::fs::File::create(&big)
            .and_then(|file| file.set_len(MAX_ATTACHMENT_BYTES + 1))
            .expect("sparse file");
        let roots = crate::SendRoots::new([dir.path().to_path_buf()]).expect("roots");
        let file = roots.admit(&big).expect("admitted");
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let err = signal
            .upload(&file)
            .await
            .expect_err("uploaded past the limit");
        assert!(
            err.to_string().contains("larger than"),
            "unclear error: {err:#}"
        );
    }

    /// Sending to a group the account isn't in must fail with a reason, before
    /// anything reaches Signal, as it does on FakeSignal.
    #[tokio::test]
    async fn sending_to_an_unknown_group_fails_without_the_network() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let err = signal
            .send(
                Recipient::Group(GroupId::new([7; 32])),
                Outgoing::Typing { started: true },
            )
            .await
            .expect_err("sent to a group we aren't in");
        assert!(
            err.to_string().contains("not a member"),
            "unclear error: {err:#}"
        );
    }

    /// Linking wipes presage's store before it starts. Pointed at a profile
    /// that already has an account it must refuse, not silently unlink it.
    #[tokio::test]
    async fn linking_refuses_a_store_that_already_has_an_account() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let provisioner = PresageProvisioner::open(&path, &KEY, Servers::Staging)
            .await
            .expect("opens");
        let events: Vec<_> = provisioner.link("Claude (test)".into()).collect().await;
        assert!(
            matches!(events.as_slice(), [Err(_)]),
            "linking over an account produced {events:?}"
        );
        let still = PresageSignal::open(&path, &KEY).await;
        assert!(still.is_ok(), "the existing account was erased");
    }

    /// Registering a number over an existing account would orphan it; the
    /// request must fail and leave the account in place.
    #[tokio::test]
    async fn registering_refuses_a_store_that_already_has_an_account() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let provisioner = PresageProvisioner::open(&path, &KEY, Servers::Staging)
            .await
            .expect("opens");
        let err = provisioner
            .request_code("+15550000002", None, false)
            .await
            .expect_err("registered over an existing account");
        assert!(
            format!("{err:#}").contains("already"),
            "failed for the wrong reason: {err:#}"
        );
        let still = PresageSignal::open(&path, &KEY).await;
        assert!(still.is_ok(), "the existing account was erased");
    }

    /// A code typed before one was requested has no session to verify against;
    /// it must say so instead of reaching Signal with nothing.
    #[tokio::test]
    async fn verifying_before_requesting_a_code_fails_clearly() {
        let (_dir, path) = temp_store();
        let provisioner = PresageProvisioner::open(&path, &KEY, Servers::Staging)
            .await
            .expect("opens");
        let err = provisioner
            .verify_code("123456")
            .await
            .expect_err("verified with no code requested");
        assert!(
            err.to_string().contains("request a code first"),
            "unclear error: {err:#}"
        );
    }

    /// Signal identifies a message by author and timestamp, so two sends in
    /// the same millisecond must still get distinct timestamps, or the second
    /// would be taken as a duplicate of the first.
    #[test]
    fn send_timestamps_never_repeat_within_a_millisecond() {
        let last = AtomicU64::new(0);
        let first = next_timestamp(&last, 1_000);
        let second = next_timestamp(&last, 1_000);
        let earlier_clock = next_timestamp(&last, 999);
        assert!(
            first < second && second < earlier_clock,
            "timestamps repeated or went backwards: {first}, {second}, {earlier_clock}"
        );
    }

    fn group_message_from(sender: uuid::Uuid, master_key: [u8; 32]) -> Content {
        let at = chrono::Utc::now();
        Content {
            metadata: Metadata {
                sender: convert::aci(ServiceId::new(sender)).into(),
                destination: convert::aci(ServiceId::parse(ME).expect("uuid")).into(),
                sender_device: presage::libsignal_service::protocol::DeviceId::new(1)
                    .expect("device 1"),
                pni_verified: None,
                client_timestamp: at,
                server_timestamp: at,
                needs_receipt: false,
                unidentified_sender: true,
                was_plaintext: false,
                server_guid: None,
            },
            body: ContentBody::DataMessage(presage::proto::DataMessage {
                body: Some("hello group".into()),
                timestamp: Some(1),
                group_v2: Some(GroupContextV2 {
                    master_key: Some(master_key.to_vec()),
                    revision: Some(1),
                    group_change: None,
                }),
                ..Default::default()
            }),
        }
    }

    /// Someone removed from a group still knows its master key. A message from
    /// a non-member of a group is dropped, not stored as group traffic, or a
    /// former member could put words in the group's mouth.
    #[tokio::test]
    async fn message_from_a_non_member_of_a_group_is_dropped() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let me = uuid::Uuid::parse_str(ME).expect("uuid");
        let (member, outsider) = (uuid::Uuid::from_u128(0xa11ce), uuid::Uuid::from_u128(0xbad));
        seed(&path, move |store| async move {
            store
                .save_group([4; 32], cached_group("cats", &[me, member]))
                .await
                .expect("saved");
        })
        .await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let store = signal.manager.store().clone();
        let (from_member, from_outsider) = signal
            .worker
            .run(move || async move {
                (
                    admitted(&store, &group_message_from(member, [4; 32])).await,
                    admitted(&store, &group_message_from(outsider, [4; 32])).await,
                )
            })
            .await
            .expect("worker ran");
        assert!(from_member, "a member's group message was dropped");
        assert!(
            !from_outsider,
            "a non-member's message was admitted as group traffic"
        );
    }

    fn identity_key(seed: u8) -> IdentityKey {
        let mut bytes = [seed; 33];
        bytes[0] = 0x05;
        IdentityKey::decode(&bytes).expect("a curve25519 public key")
    }

    /// The store trusts a contact's new identity key silently, as Signal's
    /// apps do; the watch must still report that change exactly once, and not
    /// mistake a contact's first key or an unchanged key for one.
    #[tokio::test]
    async fn changed_identity_key_is_reported_once() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let alice = uuid::Uuid::from_u128(0xa11ce);
        seed(&path, move |mut store| async move {
            let alice_id = SignalServiceId::from(convert::aci(ServiceId::new(alice)));
            let address = ProtocolAddress::new(
                alice_id.service_id_string(),
                DeviceId::new(1).expect("device 1"),
            );
            store
                .save_contact(&presage::model::contacts::Contact {
                    uuid: alice,
                    phone_number: None,
                    name: "Alice".into(),
                    verified: Default::default(),
                    profile_key: Vec::new(),
                    expire_timer: 0,
                    expire_timer_version: 2,
                    inbox_position: 0,
                    avatar: None,
                })
                .await
                .expect("contact");
            let mut identities = store.aci_protocol_store();
            identities
                .save_identity(&address, &identity_key(1))
                .await
                .expect("first key");
            let mut watch = IdentityWatch::primed(&store).await;
            assert!(
                !watch.changed(&store, alice_id).await,
                "an unchanged key was reported"
            );
            identities
                .save_identity(&address, &identity_key(2))
                .await
                .expect("new key");
            assert!(
                watch.changed(&store, alice_id).await,
                "a new identity key went unreported"
            );
            assert!(
                !watch.changed(&store, alice_id).await,
                "one change was reported twice"
            );
        })
        .await;
    }

    type TestError = presage::Error<std::io::Error>;

    /// Signal's 429 on a send must come back as the typed RateLimited with its
    /// Retry-After, so callers tell the user when to try again instead of
    /// sending again.
    #[test]
    fn a_rate_limited_send_carries_signals_retry_after() {
        let error = TestError::from(MessageSenderError::ServiceError(
            ServiceError::RateLimitExceeded {
                retry_after: Some(chrono::Duration::seconds(30)),
            },
        ));
        let limited = rate_limited(&error).expect("a 429 went unrecognized");
        assert_eq!(
            limited.retry_after,
            Some(Duration::from_secs(30)),
            "{limited:?}"
        );
    }

    /// Signal's 428 "proof required" on a send is its rate-limit challenge: it
    /// must surface as RateLimited too, naming the proof Signal wants but not
    /// the challenge token, which is no business of the agent's client.
    #[test]
    fn a_proof_required_send_is_rate_limited_without_its_token() {
        let error = TestError::from(MessageSenderError::ProofRequired {
            token: "secret-challenge-token".into(),
            options: vec!["captcha".into()],
        });
        let limited = rate_limited(&error).expect("a 428 went unrecognized");
        assert!(
            limited.message.contains("captcha") && !limited.message.contains("secret"),
            "{limited:?}"
        );
    }

    /// Requesting or verifying a code too often gets a 429 on the verification
    /// session. It must surface as RateLimited with Signal's wait, or setup
    /// would show a generic failure and invite an immediate retry.
    #[test]
    fn a_rate_limited_verification_session_carries_signals_retry_after() {
        let session = RegistrationSessionMetadataResponse {
            id: "session".into(),
            next_sms: Some(120),
            next_call: None,
            next_verification_attempt: None,
            allowed_to_request_code: false,
            requested_information: Vec::new(),
            verified: false,
        };
        let error = TestError::ServiceError(ServiceError::VerificationSessionRateLimited {
            session,
            retry_after: Some(chrono::Duration::seconds(120)),
        });
        let limited = rate_limited(&error).expect("a session 429 went unrecognized");
        assert_eq!(
            limited.retry_after,
            Some(Duration::from_secs(120)),
            "{limited:?}"
        );
    }

    /// Linking reaches Signal through the provisioning layer, which wraps the
    /// service error once more; a 429 there must not hide as a link failure
    /// the user would just retry.
    #[test]
    fn a_rate_limited_link_is_recognized_through_provisioning() {
        let error = TestError::ProvisioningError(ProvisioningError::ServiceError(
            ServiceError::RateLimitExceeded { retry_after: None },
        ));
        let limited = rate_limited(&error).expect("a link 429 went unrecognized");
        assert_eq!(limited.retry_after, None, "{limited:?}");
    }

    /// Every send, link and code request first opens a websocket, and a 429
    /// on that upgrade arrives as a handshake error with no Retry-After. It is
    /// still Signal rate-limiting the account and must be recognized as such.
    #[test]
    fn a_rate_limited_websocket_upgrade_is_recognized() {
        let handshake = reqwest_websocket::HandshakeError::UnexpectedStatusCode(
            http::StatusCode::TOO_MANY_REQUESTS,
        );
        let error = TestError::ServiceError(ServiceError::WsError(Box::new(handshake.into())));
        let limited = rate_limited(&error).expect("a websocket 429 went unrecognized");
        assert_eq!(limited.retry_after, None, "{limited:?}");
    }

    /// Tools and setup show an error's outermost message. A rate limit must be
    /// that message and downcast to RateLimited, not sit under "unable to send"
    /// where the wait never reaches the user.
    #[test]
    fn a_rate_limit_is_the_outermost_error_callers_see() {
        let error = TestError::from(MessageSenderError::ServiceError(
            ServiceError::RateLimitExceeded {
                retry_after: Some(chrono::Duration::seconds(30)),
            },
        ));
        let error = signal_error(error, "unable to send to alice");
        assert!(
            error.downcast_ref::<RateLimited>().is_some(),
            "not typed: {error:#}"
        );
        assert!(error.to_string().contains("30s"), "wait hidden: {error}");
    }

    /// An upload reports its failure in its own error type, beside presage's.
    /// A 429 there is still Signal rate-limiting the account and must come
    /// back typed, with its wait, and outermost.
    #[test]
    fn a_rate_limited_upload_is_recognized() {
        let error = upload_error(AttachmentUploadError::ServiceError(
            ServiceError::RateLimitExceeded {
                retry_after: Some(chrono::Duration::seconds(30)),
            },
        ));
        let limited = error
            .downcast_ref::<RateLimited>()
            .unwrap_or_else(|| panic!("an upload 429 went unrecognized: {error:#}"));
        assert_eq!(limited.retry_after, Some(Duration::from_secs(30)));
    }

    /// Reconnecting to receive after a drop is fine, but reconnecting a second
    /// after Signal refused the connection as rate-limited is hammering it. A
    /// rate-limited reconnect waits the longest backoff, or Signal's own
    /// Retry-After when that is longer.
    #[test]
    fn a_rate_limited_reconnect_waits_at_least_the_longest_backoff() {
        let limited = |retry_after| RateLimited {
            message: "429".into(),
            retry_after,
        };
        let long = MAX_RECONNECT_DELAY * 4;
        let cases = [
            (None, MIN_RECONNECT_DELAY),
            (Some(limited(None)), MAX_RECONNECT_DELAY),
            (
                Some(limited(Some(Duration::from_secs(1)))),
                MAX_RECONNECT_DELAY,
            ),
            (Some(limited(Some(long))), long),
        ];
        for (limit, expected) in cases {
            let wait = reconnect_wait(MIN_RECONNECT_DELAY, limit.as_ref());
            assert_eq!(wait, expected, "after {limit:?}");
        }
    }

    /// The store must be unreadable without its key. If the SQLCipher build
    /// ever falls back to plain SQLite the key pragma is silently ignored, and
    /// this is the test that notices.
    #[tokio::test]
    async fn store_refuses_to_open_with_the_wrong_key() {
        let (_dir, path) = temp_store();
        let first = Worker::open(&path, &[1; 32])
            .await
            .expect("first open creates the store");
        drop(first);
        let wrong = Worker::open(&path, &[2; 32]).await;
        assert!(
            wrong.is_err(),
            "opened an encrypted store with the wrong key"
        );
    }

    /// A SQLCipher pool dropped without close() can crash at process exit in
    /// SQLCipher's codec cleanup. Dropping the backend must close its store
    /// before the drop returns.
    #[tokio::test]
    async fn dropping_the_backend_closes_its_store() {
        let (_dir, path) = temp_store();
        register_linked(&path).await;
        let signal = PresageSignal::open(&path, &KEY).await.expect("opens");
        let store = signal.manager.store().clone();
        drop(signal);
        let after = store.contacts().await;
        assert!(
            after.is_err(),
            "store still usable after the backend was dropped"
        );
    }
}
