//! The one process per profile that holds the Signal connection. Everything
//! else (stdio shims, the HTTP front end, setup) reaches Signal through it,
//! because two processes driving one Signal device corrupt its sessions.

mod capture;
pub mod endpoint;
pub mod login;
mod mcp;
mod startup;
mod supervisor;

use std::sync::Arc;

use anyhow::{Context, bail};
use relay_core::prelude::*;
use relay_signal::SignalBackend;
use relay_store::MessageStore;
use relay_tools::{Clock, Limiter, Limits, Registry, SystemClock, ToolContext};
use rmcp::ServiceExt;
use tokio::task::JoinSet;

pub use supervisor::{Supervisor, SupervisorAction};

/// What the agent needs to start.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub profile: ProfileName,
    pub paths: ProfilePaths,
    pub access: AccessMode,
}

/// A profile's agent around an already-open Signal backend and message store.
pub struct Agent {
    signal: Arc<dyn SignalBackend>,
    store: Arc<dyn MessageStore>,
    registry: Arc<Registry>,
    access: AccessMode,
}

impl Agent {
    pub fn new(
        signal: Arc<dyn SignalBackend>,
        store: Arc<dyn MessageStore>,
        registry: Registry,
        access: AccessMode,
    ) -> Self {
        Self {
            signal,
            store,
            registry: Arc::new(registry),
            access,
        }
    }

    /// Captures everything Signal delivers into the store and serves MCP to
    /// every connection on `listener` until `shutdown` resolves or accepting
    /// fails. Either way the store is closed and the backend dropped before
    /// this returns.
    pub async fn serve(
        self,
        listener: endpoint::Listener,
        shutdown: impl Future<Output = ()>,
    ) -> anyhow::Result<()> {
        let Agent {
            signal,
            store,
            registry,
            access,
        } = self;
        let served = serve_until(
            signal.clone(),
            store.clone(),
            registry,
            access,
            listener,
            shutdown,
        )
        .await;
        // NOTE: every task holding the store has been joined by now, so nothing
        // touches it after close. Dropping SQLCipher unclosed crashed the
        // process at exit; dropping the backend lets presage close its store.
        let closed = store
            .close()
            .await
            .context("unable to close the message store");
        drop(signal);
        served.and(closed)
    }
}

async fn serve_until(
    signal: Arc<dyn SignalBackend>,
    store: Arc<dyn MessageStore>,
    registry: Arc<Registry>,
    access: AccessMode,
    mut listener: endpoint::Listener,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let kind = signal.account().await?.kind;
    let handler = mcp::Handler {
        ctx: ToolContext {
            store: store.clone(),
            signal: signal.clone(),
            access,
            kind,
            send_roots: Arc::default(),
            webhook: None,
        },
        registry,
    };
    let capture = tokio::spawn(capture::run(signal, store));
    let mut sessions = JoinSet::new();
    tokio::pin!(shutdown);
    let result = loop {
        let stream = tokio::select! {
            () = &mut shutdown => break Ok(()),
            accepted = listener.accept() => match accepted {
                Ok(stream) => stream,
                Err(e) => break Err(e),
            },
        };
        let handler = handler.clone();
        sessions.spawn(async move {
            let session = match handler.serve(stream).await {
                Ok(session) => session,
                Err(e) => return tracing::warn!("client failed to initialize: {e}"),
            };
            if let Err(e) = session.waiting().await {
                tracing::warn!("client session ended with an error: {e}");
            }
        });
        // Reap finished sessions so a long-lived agent doesn't accumulate them.
        while sessions.try_join_next().is_some() {}
    };
    sessions.shutdown().await;
    capture.abort();
    // An aborted task reports cancellation; all that matters is that it ended.
    let _ = capture.await;
    result
}

/// Runs the agent until it is told to stop: receives continuously into the
/// store and serves MCP JSON-RPC on the profile's user-only local endpoint.
pub async fn run(config: AgentConfig) -> anyhow::Result<()> {
    // NOTE: bind before opening Signal. The bind is the single-instance check;
    // opening presage's store first would let a second agent touch the same
    // device state before discovering it should exit.
    let name = endpoint::name(&config.profile, &config.paths)?;
    let listener = endpoint::Listener::bind(&name).await?;
    let profile = config.profile.clone();
    // Off the critical path: it shells out to Claude's CLIs, and the agent must
    // start capturing whether or not that works.
    tokio::task::spawn_blocking(move || startup::reregister_claude_entries(&profile));
    let (signal, store) = open_backend_and_store(&config).await?;
    let registry = registry(&config.paths, Arc::new(SystemClock));
    Agent::new(signal, store, registry, config.access)
        .serve(listener, stop_requested())
        .await
}

/// The shipped tools under limits kept in the profile directory, so
/// restarting the agent doesn't reset them.
// NOTE: the default limits apply until they're read from settings (#72).
fn registry(paths: &ProfilePaths, clock: Arc<dyn Clock>) -> Registry {
    let limiter = Limiter::new(Limits::default(), clock).persisted(paths.root.join("limits.json"));
    Registry::standard().with_limiter(limiter)
}

/// Resolves when the OS asks the agent to stop: Ctrl-C anywhere, and SIGTERM
/// on Unix, which is how systemd and launchd stop it.
async fn stop_requested() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            tracing::warn!("unable to listen for SIGTERM; only Ctrl-C will stop the agent cleanly");
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

// Integration seam: the presage backend (lane L1) and the SQLCipher store with
// its key from relay-secrets (lane L2) are opened here once they exist. Tests
// never come through this path; they build an `Agent` around fakes.
async fn open_backend_and_store(
    config: &AgentConfig,
) -> anyhow::Result<(Arc<dyn SignalBackend>, Arc<dyn MessageStore>)> {
    bail!(
        "profile \"{}\": the presage backend and SQLCipher store are not wired into the agent yet",
        config.profile
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use relay_signal::{Account, FakeSignal, Incoming};
    use relay_store::{JobId, MemoryStore, SearchQuery};
    use rmcp::model::CallToolRequestParams;
    use uuid::Uuid;

    use super::*;
    use crate::mcp::tests::ProbeSend;

    /// A fresh endpoint name no other test uses.
    pub(crate) fn test_endpoint(dir: &std::path::Path) -> String {
        if cfg!(windows) {
            format!(r"\\.\pipe\signal-agent-test-{}", Uuid::new_v4())
        } else {
            dir.join("p").join("agent.sock").display().to_string()
        }
    }

    pub(crate) fn fake_signal() -> Arc<FakeSignal> {
        Arc::new(FakeSignal::new(Account {
            service_id: ServiceId::new(Uuid::from_u128(1)),
            phone_number: None,
            kind: ProfileKind::Linked,
            device_name: "test".into(),
        }))
    }

    struct FakeClock(std::sync::atomic::AtomicU64);

    impl relay_tools::Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl FakeClock {
        fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// An AI client that can crash or restart the agent must not get a fresh
    /// day of first contacts out of it: the agent's limits live on disk.
    #[tokio::test]
    async fn the_new_recipient_cap_holds_across_an_agent_restart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let profile = ProfileName::new("default").expect("valid profile");
        let paths = ProfilePaths::under(tmp.path().to_owned(), &profile);
        std::fs::create_dir_all(&paths.root).expect("profile dir");
        let clock = Arc::new(FakeClock(0.into()));
        let signal = fake_signal();
        let ctx = ToolContext {
            store: Arc::new(MemoryStore::default()),
            signal: signal.clone(),
            access: AccessMode::Full,
            kind: ProfileKind::Linked,
            send_roots: Arc::default(),
            webhook: None,
        };
        let dm = |n: u128| {
            let to = ServiceId::new(Uuid::from_u128(n)).to_string();
            serde_json::json!({"recipient": to, "message": "hi"})
        };
        let agent = registry(&paths, clock.clone());
        for n in 10..30 {
            let result = agent.call(&ctx, "send_message", dm(n)).await;
            assert!(
                result.is_ok(),
                "new recipient {n} refused under the cap: {result:?}"
            );
            clock.advance(3_000);
        }
        let restarted = registry(&paths, clock.clone());
        let result = restarted.call(&ctx, "send_message", dm(30)).await;
        let Err(relay_tools::ToolError::Internal(e)) = &result else {
            panic!("21st new recipient after a restart was not refused: {result:?}");
        };
        let refusal = e.downcast_ref::<relay_tools::LimitExceeded>();
        assert!(
            refusal.is_some_and(|r| r.limit.contains("new conversations")),
            "refused for the wrong reason: {e:#}"
        );
        assert_eq!(
            signal.sent().len(),
            20,
            "a refused first contact reached Signal"
        );
    }

    /// A damaged limits file must not keep the agent from starting, since that
    /// would stop message capture too: it still builds its tools, read tools
    /// answer, and sending is refused instead of the agent going down.
    #[tokio::test]
    async fn a_damaged_limits_file_doesnt_stop_the_agent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let profile = ProfileName::new("default").expect("valid profile");
        let paths = ProfilePaths::under(tmp.path().to_owned(), &profile);
        std::fs::create_dir_all(&paths.root).expect("profile dir");
        std::fs::write(paths.root.join("limits.json"), b"\0\0garbage").expect("damage file");
        let signal = fake_signal();
        let ctx = ToolContext {
            store: Arc::new(MemoryStore::default()),
            signal: signal.clone(),
            access: AccessMode::Full,
            kind: ProfileKind::Linked,
            send_roots: Arc::default(),
            webhook: None,
        };
        let registry = registry(&paths, Arc::new(FakeClock(0.into())));
        let stats = registry
            .call(&ctx, "store_stats", serde_json::json!({}))
            .await;
        assert!(
            stats.is_ok(),
            "read tool failed after a damaged limits file: {stats:?}"
        );
        let to = ServiceId::new(Uuid::from_u128(2)).to_string();
        let sent = registry
            .call(
                &ctx,
                "send_message",
                serde_json::json!({"recipient": to, "message": "hi"}),
            )
            .await;
        let Err(relay_tools::ToolError::Internal(e)) = &sent else {
            panic!("a send after a damaged limits file wasn't refused: {sent:?}");
        };
        assert!(
            e.downcast_ref::<relay_tools::LimitExceeded>().is_some(),
            "refused for the wrong reason: {e:#}"
        );
        assert!(signal.sent().is_empty(), "a refused send reached Signal");
    }

    /// Wraps a MemoryStore and records whether close() was awaited.
    #[derive(Default)]
    struct ClosingStore {
        inner: MemoryStore,
        closed: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl MessageStore for ClosingStore {
        async fn insert(&self, m: &Message) -> anyhow::Result<bool> {
            self.inner.insert(m).await
        }
        async fn apply_edit(&self, t: MessageId, e: ServiceId, b: &str) -> anyhow::Result<()> {
            self.inner.apply_edit(t, e, b).await
        }
        async fn mark_deleted(&self, t: MessageId, d: ServiceId) -> anyhow::Result<()> {
            self.inner.mark_deleted(t, d).await
        }
        async fn mark_read(&self, t: &[MessageId]) -> anyhow::Result<()> {
            self.inner.mark_read(t).await
        }
        async fn mark_unread(&self, t: &[MessageId]) -> anyhow::Result<()> {
            self.inner.mark_unread(t).await
        }
        async fn prune(&self, older_than_ms: u64) -> anyhow::Result<u64> {
            self.inner.prune(older_than_ms).await
        }
        async fn export(
            &self,
            c: Option<Recipient>,
            s: Option<u64>,
        ) -> anyhow::Result<Vec<Message>> {
            self.inner.export(c, s).await
        }
        async fn set_title(&self, c: Recipient, t: &str) -> anyhow::Result<()> {
            self.inner.set_title(c, t).await
        }
        async fn get(&self, id: MessageId) -> anyhow::Result<Option<Message>> {
            self.inner.get(id).await
        }
        async fn conversations(&self) -> anyhow::Result<Vec<Conversation>> {
            self.inner.conversations().await
        }
        async fn history(
            &self,
            c: Recipient,
            b: Option<u64>,
            l: u32,
        ) -> anyhow::Result<Vec<Message>> {
            self.inner.history(c, b, l).await
        }
        async fn search(&self, q: &SearchQuery) -> anyhow::Result<Vec<Message>> {
            self.inner.search(q).await
        }
        async fn unread(&self) -> anyhow::Result<Vec<Message>> {
            self.inner.unread().await
        }
        async fn stats(&self) -> anyhow::Result<relay_store::StoreStats> {
            self.inner.stats().await
        }
        async fn delete_conversation(&self, c: Recipient) -> anyhow::Result<u64> {
            self.inner.delete_conversation(c).await
        }
        async fn mark_admin_deleted(&self, g: GroupId, t: MessageId) -> anyhow::Result<()> {
            self.inner.mark_admin_deleted(g, t).await
        }
        async fn attachments(
            &self,
            l: u32,
            o: u32,
        ) -> anyhow::Result<Vec<relay_store::StoredAttachment>> {
            self.inner.attachments(l, o).await
        }
        async fn attachment(
            &self,
            id: &str,
        ) -> anyhow::Result<Option<relay_store::StoredAttachment>> {
            self.inner.attachment(id).await
        }
        async fn next_poll_vote(&self, p: MessageId) -> anyhow::Result<u32> {
            self.inner.next_poll_vote(p).await
        }
        async fn schedule(&self, s: &relay_store::ScheduledSend) -> anyhow::Result<JobId> {
            self.inner.schedule(s).await
        }
        async fn scheduled(&self) -> anyhow::Result<Vec<relay_store::ScheduledJob>> {
            self.inner.scheduled().await
        }
        async fn cancel_scheduled(&self, id: JobId) -> anyhow::Result<bool> {
            self.inner.cancel_scheduled(id).await
        }
        async fn take_due(&self, now: u64) -> anyhow::Result<Vec<relay_store::ScheduledJob>> {
            self.inner.take_due(now).await
        }
        async fn finish_scheduled(&self, id: JobId, r: Result<(), String>) -> anyhow::Result<()> {
            self.inner.finish_scheduled(id, r).await
        }
        async fn close(&self) -> anyhow::Result<()> {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// Dropping the SQLCipher store unclosed crashed the process at exit, so a
    /// requested shutdown must end serve() only after store.close() returned.
    #[tokio::test]
    async fn shutdown_closes_the_store_before_serve_returns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = test_endpoint(tmp.path());
        let store = Arc::new(ClosingStore::default());
        let agent = Agent::new(
            fake_signal(),
            store.clone(),
            Registry::default(),
            AccessMode::Full,
        );
        let listener = endpoint::Listener::bind(&name).await.expect("bind");
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let served = tokio::spawn(agent.serve(listener, async {
            let _ = stop_rx.await;
        }));
        let _client = endpoint::connect(&name).await.expect("connect");
        stop_tx.send(()).expect("agent still running");
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), served)
            .await
            .expect("serve did not stop")
            .expect("serve panicked");
        assert!(result.is_ok(), "clean shutdown returned {result:?}");
        assert!(
            store.closed.load(std::sync::atomic::Ordering::SeqCst),
            "store dropped without close()"
        );
    }

    /// A backend that is offline from the start.
    struct OfflineSignal;

    #[async_trait::async_trait]
    impl SignalBackend for OfflineSignal {
        async fn account(&self) -> anyhow::Result<relay_signal::Account> {
            anyhow::bail!("offline")
        }
        async fn send(&self, _: Recipient, _: relay_signal::Outgoing) -> anyhow::Result<MessageId> {
            anyhow::bail!("offline")
        }
        async fn contacts(&self) -> anyhow::Result<Vec<relay_signal::Contact>> {
            anyhow::bail!("offline")
        }
        async fn groups(&self) -> anyhow::Result<Vec<relay_signal::Group>> {
            anyhow::bail!("offline")
        }
        async fn attachment(&self, _: &AttachmentRef) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("offline")
        }
        fn incoming(&self) -> futures::stream::BoxStream<'static, anyhow::Result<Incoming>> {
            Box::pin(futures::stream::empty())
        }
        async fn upload(&self, _: &relay_signal::SendableFile) -> anyhow::Result<AttachmentRef> {
            anyhow::bail!("offline")
        }
        async fn discover(&self, _: &[String]) -> anyhow::Result<Vec<(String, Option<ServiceId>)>> {
            anyhow::bail!("offline")
        }
        async fn lookup_username(&self, _: &str) -> anyhow::Result<Option<ServiceId>> {
            anyhow::bail!("offline")
        }
        async fn avatar(&self, _: Recipient) -> anyhow::Result<Option<Vec<u8>>> {
            anyhow::bail!("offline")
        }
        async fn identities(&self) -> anyhow::Result<Vec<relay_signal::Identity>> {
            anyhow::bail!("offline")
        }
        async fn devices(&self) -> anyhow::Result<Vec<relay_signal::Device>> {
            anyhow::bail!("offline")
        }
    }

    /// An agent that fails after its store opened must still close the store,
    /// or the error exit is where the SQLCipher crash would happen.
    #[tokio::test]
    async fn a_failed_start_still_closes_the_store() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = test_endpoint(tmp.path());
        let store = Arc::new(ClosingStore::default());
        let agent = Agent::new(
            Arc::new(OfflineSignal),
            store.clone(),
            Registry::default(),
            AccessMode::Full,
        );
        let listener = endpoint::Listener::bind(&name).await.expect("bind");
        let result = agent.serve(listener, std::future::pending()).await;
        assert!(result.is_err(), "an offline backend started serving");
        assert!(
            store.closed.load(std::sync::atomic::Ordering::SeqCst),
            "store left open after a failed start"
        );
    }

    /// The agent must capture and serve at once: a client's send reaches Signal
    /// over the endpoint while an incoming message lands in the store.
    #[tokio::test]
    async fn the_agent_serves_mcp_on_its_endpoint_while_capturing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = test_endpoint(tmp.path());
        let signal = fake_signal();
        let store = Arc::new(MemoryStore::default());
        let agent = Agent::new(
            signal.clone(),
            store.clone(),
            Registry::default().with(ProbeSend),
            AccessMode::Full,
        );
        let listener = endpoint::Listener::bind(&name).await.expect("bind");
        let served = tokio::spawn(agent.serve(listener, std::future::pending()));

        let stream = endpoint::connect(&name).await.expect("connect");
        let client = ().serve(stream).await.expect("initialize");
        let args = serde_json::json!({"body": "hi"});
        let serde_json::Value::Object(args) = args else {
            unreachable!()
        };
        let result = client
            .peer()
            .call_tool(CallToolRequestParams::new("probe_send").with_arguments(args))
            .await
            .expect("tools/call");
        assert_ne!(result.is_error, Some(true), "send failed: {result:?}");
        assert_eq!(signal.sent().len(), 1, "send did not reach Signal");

        let sender = ServiceId::new(Uuid::from_u128(2));
        signal.push(Incoming::Message(Message {
            id: MessageId {
                author: sender,
                sent_at_ms: 10,
            },
            conversation: Recipient::Contact(sender),
            sender,
            from_me: false,
            received_at_ms: 10,
            body: Some("captured".into()),
            attachments: vec![],
            quote: None,
            edited: false,
            deleted: false,
        }));
        let query = SearchQuery {
            text: "captured".into(),
            limit: 1,
            ..Default::default()
        };
        let mut found = vec![];
        for _ in 0..200 {
            found = store.search(&query).await.expect("search");
            if !found.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        served.abort();
        assert_eq!(
            found.len(),
            1,
            "incoming message not captured while serving"
        );
    }
}
