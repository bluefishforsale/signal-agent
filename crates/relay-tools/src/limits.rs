//! Account-safety limits. Signal flags accounts that look automated: high send
//! rates and many first contacts. An AI client can loop, and every client goes
//! through this agent, so the brakes live here rather than in each tool.

use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use async_trait::async_trait;
use futures::stream::BoxStream;
use relay_core::prelude::*;
use relay_signal::{Account, Contact, Group, Incoming, Outgoing, SignalBackend};
use relay_store::MessageStore;
use serde::{Deserialize, Serialize};

use crate::ToolContext;

const MINUTE_MS: u64 = 60_000;
const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;

/// How much a profile may send before tools refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Messages allowed back to back, refilling over a minute.
    pub burst_per_minute: NonZeroU32,
    /// Messages allowed back to back, refilling over an hour.
    pub per_hour: NonZeroU32,
    /// Conversations with no history that may get a first message in any
    /// 24 hours. A group is one conversation.
    pub new_recipients_per_day: u32,
    /// Minutes between attempts to link or register this profile.
    pub provisioning_backoff_minutes: NonZeroU32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            burst_per_minute: NonZeroU32::new(20).expect("nonzero literal"),
            per_hour: NonZeroU32::new(200).expect("nonzero literal"),
            new_recipients_per_day: 20,
            provisioning_backoff_minutes: NonZeroU32::new(5).expect("nonzero literal"),
        }
    }
}

/// Milliseconds since the Unix epoch, injectable so tests control time.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// The wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }
}

/// A refusal because an account-safety limit was reached.
#[derive(Debug, thiserror::Error)]
#[error(
    "account-safety limit reached: {limit}. Nothing was sent to Signal; try again in {}s",
    retry_after.as_secs()
)]
pub struct LimitExceeded {
    pub limit: String,
    pub retry_after: Duration,
}

/// One profile's send budget.
pub struct Limiter {
    limits: Limits,
    clock: Arc<dyn Clock>,
    path: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    minute: Bucket,
    hour: Bucket,
    /// Conversations first messaged in the last day, oldest first.
    first_contacts: Vec<(Recipient, u64)>,
    /// No first contacts before this time.
    first_contacts_paused_until_ms: u64,
}

impl State {
    /// Every budget spent as of `now`, for when the real count is unknown.
    fn exhausted(now: u64) -> Self {
        let empty = || Bucket {
            level: 0,
            last_ms: Some(now),
        };
        Self {
            minute: empty(),
            hour: empty(),
            first_contacts: vec![],
            first_contacts_paused_until_ms: now + DAY_MS,
        }
    }
}

/// A token bucket in exact integer units: one token is `window_ms` units, and
/// it refills `capacity` units per millisecond, so a full bucket takes one
/// window to refill from empty.
#[derive(Default, Serialize, Deserialize)]
struct Bucket {
    level: u64,
    last_ms: Option<u64>,
}

impl Bucket {
    /// How long until a token is free, or None when one is free now.
    fn wait(&mut self, now: u64, capacity: NonZeroU32, window_ms: u64) -> Option<Duration> {
        let capacity = u64::from(capacity.get());
        let full = capacity * window_ms;
        let refill = match self.last_ms {
            None => full,
            Some(last) => now.saturating_sub(last).saturating_mul(capacity),
        };
        self.level = self.level.saturating_add(refill).min(full);
        self.last_ms = Some(now);
        let short = window_ms.checked_sub(self.level).filter(|s| *s > 0)?;
        Some(Duration::from_millis(short.div_ceil(capacity)))
    }

    fn take(&mut self, window_ms: u64) {
        self.level -= window_ms;
    }
}

impl Limiter {
    pub fn new(limits: Limits, clock: Arc<dyn Clock>) -> Self {
        Self {
            limits,
            clock,
            path: None,
            state: Mutex::default(),
        }
    }

    /// Keeps the send budget and new-recipient count in `path`, so a restart
    /// doesn't reset them.
    // NOTE: an unreadable file must not stop the agent, which would also stop
    // message capture. The limiter starts with every budget spent instead, and
    // that state replaces the file in one step: a moment with no file at all
    // would let a restart load a fresh budget.
    pub fn persisted(mut self, path: PathBuf) -> Self {
        let state = match load(&path) {
            Ok(state) => state,
            Err(e) => {
                let now = self.clock.now_ms();
                let state = State::exhausted(now);
                match replace_damaged(&path, &state, now) {
                    Ok(Some(aside)) => tracing::warn!(
                        "{e:#}; kept it as {} and paused sending until the limits refill",
                        aside.display()
                    ),
                    Ok(None) => tracing::warn!("{e:#}; paused sending until the limits refill"),
                    Err(r) => tracing::warn!(
                        "{e:#}; left it in place ({r:#}) and paused sending until the limits refill"
                    ),
                }
                state
            }
        };
        *self.state.get_mut().expect("limiter state") = state;
        self.path = Some(path);
        self
    }

    /// Counts one message to `to`, or refuses it.
    async fn admit(&self, store: &dyn MessageStore, to: Recipient) -> anyhow::Result<()> {
        let known = !store.history(to, None, 1).await?.is_empty();
        let now = self.clock.now_ms();
        let mut state = self.state.lock().expect("limiter state");
        let Limits {
            burst_per_minute,
            per_hour,
            new_recipients_per_day,
            ..
        } = self.limits;
        state
            .first_contacts
            .retain(|(_, at)| now.saturating_sub(*at) < DAY_MS);
        let first = !known && !state.first_contacts.iter().any(|(r, _)| *r == to);
        if first && now < state.first_contacts_paused_until_ms {
            return Err(LimitExceeded {
                limit: "no new conversations while the limits recover from a damaged \
                        limits file"
                    .into(),
                retry_after: Duration::from_millis(state.first_contacts_paused_until_ms - now),
            }
            .into());
        }
        if first && state.first_contacts.len() >= new_recipients_per_day as usize {
            let oldest = state.first_contacts.first().map_or(now, |(_, at)| *at);
            return Err(LimitExceeded {
                limit: format!(
                    "at most {new_recipients_per_day} new conversations a day; \
                     this one has no history yet"
                ),
                retry_after: Duration::from_millis(oldest + DAY_MS - now),
            }
            .into());
        }
        // Report the longer wait, so a client that retries when told isn't
        // refused again by the other bucket.
        let waits = [
            (
                state.minute.wait(now, burst_per_minute, MINUTE_MS),
                format!("at most {burst_per_minute} messages a minute"),
            ),
            (
                state.hour.wait(now, per_hour, HOUR_MS),
                format!("at most {per_hour} messages an hour"),
            ),
        ];
        if let Some((Some(retry_after), limit)) = waits.into_iter().max_by_key(|(w, _)| *w) {
            return Err(LimitExceeded { limit, retry_after }.into());
        }
        state.minute.take(MINUTE_MS);
        state.hour.take(HOUR_MS);
        if first {
            state.first_contacts.push((to, now));
        }
        // NOTE: saved before the send, so a crash mid-send costs a token
        // rather than handing one back.
        if let Some(path) = &self.path {
            write_atomically(path, &serde_json::to_vec(&*state)?)?;
        }
        Ok(())
    }
}

/// Records an attempt to link or register at `now_ms` in `path`, or refuses
/// it when the previous attempt was within the backoff. Call it before every
/// `Provisioner::link` and `Provisioner::request_code`.
pub fn begin_provisioning(path: &Path, limits: &Limits, now_ms: u64) -> anyhow::Result<()> {
    let last = match std::fs::read(path) {
        Ok(bytes) => Some(
            serde_json::from_slice::<u64>(&bytes)
                .with_context(|| format!("unreadable provisioning file {}", path.display()))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("can't read {}", path.display())),
    };
    let minutes = limits.provisioning_backoff_minutes;
    if let Some(retry_after) = provisioning_wait(last, now_ms, minutes) {
        return Err(LimitExceeded {
            limit: format!("one link or registration attempt every {minutes} minutes"),
            retry_after,
        }
        .into());
    }
    write_atomically(path, &serde_json::to_vec(&now_ms)?)
}

/// How long until another attempt is allowed after one at `last_ms`.
fn provisioning_wait(last_ms: Option<u64>, now_ms: u64, minutes: NonZeroU32) -> Option<Duration> {
    let until = last_ms? + u64::from(minutes.get()) * MINUTE_MS;
    let wait = until.checked_sub(now_ms).filter(|w| *w > 0)?;
    Some(Duration::from_millis(wait))
}

/// The saved state at `path`, or a fresh one if there's none yet.
fn load(path: &Path) -> anyhow::Result<State> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("unreadable limits file {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match damaged_sibling(path) {
            Some(aside) => anyhow::bail!(
                "{} is missing but a damaged one was set aside as {}",
                path.display(),
                aside.display()
            ),
            None => Ok(State::default()),
        },
        Err(e) => Err(e).with_context(|| format!("can't read {}", path.display())),
    }
}

/// A damaged limits file set aside next to `path`, if any.
fn damaged_sibling(path: &Path) -> Option<PathBuf> {
    let prefix = format!("{}.corrupt-", path.file_name()?.to_string_lossy());
    std::fs::read_dir(path.parent()?)
        .ok()?
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path())
}

/// Copies the damaged file at `path` aside, then replaces it with `state`,
/// returning where the copy went. On any failure the damaged file stays where
/// it is, so the next start takes the same exhausted path.
fn replace_damaged(path: &Path, state: &State, now: u64) -> anyhow::Result<Option<PathBuf>> {
    // A missing file was judged damaged because a set-aside copy exists, so
    // there's nothing new to keep.
    let aside = if path.exists() {
        let mut aside = path.as_os_str().to_owned();
        aside.push(format!(".corrupt-{now}"));
        let aside = PathBuf::from(aside);
        std::fs::copy(path, &aside)
            .with_context(|| format!("can't copy it to {}", aside.display()))?;
        Some(aside)
    } else {
        None
    };
    write_atomically(path, &serde_json::to_vec(state)?)?;
    Ok(aside)
}

// NOTE: a torn write would pause sending for a day on the next start, so write
// beside the file and rename over it.
fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("can't write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("can't replace {}", path.display()))
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new(Limits::default(), Arc::new(SystemClock))
    }
}

/// `ctx` with its Signal connection routed through `limiter`.
pub(crate) fn guard(limiter: Option<&Arc<Limiter>>, ctx: &ToolContext) -> ToolContext {
    let Some(limiter) = limiter else {
        return ctx.clone();
    };
    ToolContext {
        signal: Arc::new(Guarded {
            inner: ctx.signal.clone(),
            store: ctx.store.clone(),
            limiter: limiter.clone(),
        }),
        ..ctx.clone()
    }
}

// NOTE: limiting at the backend rather than per tool means a new tool that
// sends is limited without opting in, whatever its writes() says.
struct Guarded {
    inner: Arc<dyn SignalBackend>,
    store: Arc<dyn MessageStore>,
    limiter: Arc<Limiter>,
}

#[async_trait]
impl SignalBackend for Guarded {
    async fn account(&self) -> anyhow::Result<Account> {
        self.inner.account().await
    }
    async fn send(&self, to: Recipient, message: Outgoing) -> anyhow::Result<MessageId> {
        if counts(&message) {
            self.limiter.admit(self.store.as_ref(), to).await?;
        }
        self.inner.send(to, message).await
    }
    async fn contacts(&self) -> anyhow::Result<Vec<Contact>> {
        self.inner.contacts().await
    }
    async fn groups(&self) -> anyhow::Result<Vec<Group>> {
        self.inner.groups().await
    }
    async fn attachment(&self, attachment: &AttachmentRef) -> anyhow::Result<Vec<u8>> {
        self.inner.attachment(attachment).await
    }
    fn incoming(&self) -> BoxStream<'static, anyhow::Result<Incoming>> {
        self.inner.incoming()
    }
}

/// Messages a person sees count; typing indicators and read receipts are
/// background chatter every Signal client sends freely.
fn counts(message: &Outgoing) -> bool {
    match message {
        Outgoing::Text { .. }
        | Outgoing::Reaction { .. }
        | Outgoing::Edit { .. }
        | Outgoing::Delete { .. } => true,
        Outgoing::Typing { .. } | Outgoing::ReadReceipt { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use proptest::prelude::*;
    use serde_json::json;

    use relay_signal::FakeSignal;

    use super::*;
    use crate::{
        Registry, ToolError,
        testing::{ME, incoming, sid},
    };

    #[derive(Default)]
    struct FakeClock(AtomicU64);

    impl FakeClock {
        fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn limited(limits: Limits) -> (Registry, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::default());
        let registry = Registry::standard().with_limiter(Limiter::new(limits, clock.clone()));
        (registry, clock)
    }

    const GROUP: &str = "abababababababababababababababababababababababababababababababab";

    /// A full-access primary profile that belongs to `GROUP`.
    fn ctx() -> (ToolContext, Arc<FakeSignal>) {
        let (ctx, _) = crate::testing::ctx(AccessMode::Full, ProfileKind::Primary);
        let account = Account {
            service_id: sid(ME),
            phone_number: None,
            kind: ProfileKind::Primary,
            device_name: "test".into(),
        };
        let signal = Arc::new(FakeSignal::new(account).with_groups(vec![Group {
            id: GroupId::parse(GROUP).expect("valid group id"),
            title: "g".into(),
            members: vec![sid(ME), sid(2)],
        }]));
        let ctx = ToolContext {
            signal: signal.clone(),
            ..ctx
        };
        (ctx, signal)
    }

    fn refused(result: &Result<serde_json::Value, ToolError>) -> Option<&LimitExceeded> {
        match result {
            Err(ToolError::Internal(e)) => e.downcast_ref(),
            _ => None,
        }
    }

    /// The 21st message inside a minute must be refused without reaching
    /// Signal, and the refusal must say when to retry.
    #[tokio::test]
    async fn a_burst_past_the_minute_limit_is_refused_and_sends_nothing() {
        let (registry, clock) = limited(Limits::default());
        let (ctx, signal) = ctx();
        let input = json!({"recipient": sid(2).to_string(), "message": "hi"});
        for i in 0..20 {
            let result = registry.call(&ctx, "send_message", input.clone()).await;
            assert!(result.is_ok(), "send {i} refused: {result:?}");
        }
        let result = registry.call(&ctx, "send_message", input.clone()).await;
        let Some(e) = refused(&result) else {
            panic!("21st send in a minute was not refused: {result:?}");
        };
        assert_eq!(e.retry_after, Duration::from_secs(3), "{e}");
        assert_eq!(signal.sent().len(), 20, "a refused send reached Signal");
        clock.advance(3_000);
        let result = registry.call(&ctx, "send_message", input).await;
        assert!(
            result.is_ok(),
            "send after the retry time refused: {result:?}"
        );
    }

    /// The registry the agent ships must be limited without anyone wiring it,
    /// so a forgotten setting can't leave an account unprotected.
    #[tokio::test]
    async fn the_shipped_registry_limits_sends_by_default() {
        let registry = Registry::standard();
        let (ctx, signal) = ctx();
        let input = json!({"recipient": sid(2).to_string(), "message": "hi"});
        let mut results = vec![];
        for _ in 0..21 {
            results.push(registry.call(&ctx, "send_message", input.clone()).await);
        }
        assert!(
            results.iter().any(|r| refused(r).is_some()),
            "21 sends in a row all went through: {results:?}"
        );
        assert_eq!(signal.sent().len(), 20);
    }

    /// Typing indicators and read receipts aren't messages anyone reads, so an
    /// exhausted message budget must not block them.
    #[tokio::test]
    async fn typing_and_read_receipts_go_through_once_the_message_budget_is_spent() {
        let one = NonZeroU32::new(1).expect("nonzero literal");
        let (registry, _clock) = limited(Limits {
            burst_per_minute: one,
            ..Limits::default()
        });
        let (ctx, signal) = ctx();
        let to = sid(2).to_string();
        let message = json!({"recipient": to, "message": "hi"});
        assert!(
            registry
                .call(&ctx, "send_message", message.clone())
                .await
                .is_ok()
        );
        let result = registry.call(&ctx, "send_message", message).await;
        assert!(
            refused(&result).is_some(),
            "budget of one allowed two: {result:?}"
        );
        for (name, input) in [
            ("set_typing", json!({"recipient": to})),
            (
                "send_read_receipt",
                json!({"sender": to, "timestamps": [1]}),
            ),
        ] {
            let result = registry.call(&ctx, name, input).await;
            assert!(
                result.is_ok(),
                "{name} blocked by the message budget: {result:?}"
            );
        }
        assert_eq!(signal.sent().len(), 3, "{:?}", signal.sent());
    }

    /// Only first contact with a stranger counts against the daily cap: a
    /// group counts once however often it's messaged, people already in the
    /// history don't count, and the cap frees up a day after each first contact.
    #[tokio::test]
    async fn the_new_recipient_cap_counts_groups_once_and_resets_daily() {
        let roomy = NonZeroU32::new(1_000).expect("nonzero literal");
        let (registry, clock) = limited(Limits {
            burst_per_minute: roomy,
            per_hour: roomy,
            ..Limits::default()
        });
        let (ctx, signal) = ctx();
        ctx.store
            .insert(&incoming(50, 1, "hey"))
            .await
            .expect("seed");
        let dm = |n: u128| json!({"recipient": sid(n).to_string(), "message": "hi"});
        let group = json!({"group_id": GROUP, "message": "hi"});
        for _ in 0..3 {
            let result = registry
                .call(&ctx, "send_group_message", group.clone())
                .await;
            assert!(result.is_ok(), "group send refused: {result:?}");
            clock.advance(1_000);
        }
        for n in 10..29 {
            let result = registry.call(&ctx, "send_message", dm(n)).await;
            assert!(
                result.is_ok(),
                "new recipient {n} refused under the cap: {result:?}"
            );
        }
        let result = registry.call(&ctx, "send_message", dm(50)).await;
        assert!(result.is_ok(), "known contact counted as new: {result:?}");
        let before = signal.sent().len();
        let result = registry.call(&ctx, "send_message", dm(30)).await;
        let Some(e) = refused(&result) else {
            panic!("21st new recipient in a day was not refused: {result:?}");
        };
        assert_eq!(
            signal.sent().len(),
            before,
            "a refused first contact reached Signal"
        );
        assert_eq!(e.retry_after, Duration::from_millis(DAY_MS - 3_000), "{e}");
        clock.advance(DAY_MS);
        let result = registry.call(&ctx, "send_message", dm(30)).await;
        assert!(
            result.is_ok(),
            "the cap did not reset after a day: {result:?}"
        );
    }

    /// Restarting the agent must not refill the send buckets, or a client that
    /// can crash the agent gets a fresh burst each time.
    #[tokio::test]
    async fn the_send_buckets_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("relay-limits-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("limits.json");
        let clock = Arc::new(FakeClock::default());
        let start = || Limiter::new(Limits::default(), clock.clone()).persisted(path.clone());
        let (ctx, signal) = ctx();
        let input = json!({"recipient": sid(2).to_string(), "message": "hi"});
        let registry = Registry::standard().with_limiter(start());
        for i in 0..20 {
            let result = registry.call(&ctx, "send_message", input.clone()).await;
            assert!(result.is_ok(), "send {i} refused: {result:?}");
        }
        let registry = Registry::standard().with_limiter(start());
        let result = registry.call(&ctx, "send_message", input).await;
        assert!(
            refused(&result).is_some(),
            "a restart refilled the minute bucket: {result:?}"
        );
        assert_eq!(signal.sent().len(), 20, "a refused send reached Signal");
        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }

    /// A damaged limits file can't say how much was already sent, so the
    /// limiter must start as if every budget were spent: known conversations
    /// wait for a token, first contacts wait a day, and the damaged file is
    /// kept aside rather than deleted.
    #[tokio::test]
    async fn after_a_damaged_limits_file_sends_are_refused_until_the_window_passes() {
        let dir = std::env::temp_dir().join(format!("relay-limits-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("limits.json");
        std::fs::write(&path, b"{\"minute\": tru").expect("write damaged file");
        let clock = Arc::new(FakeClock::default());
        clock.advance(1_000_000);
        let limiter = Limiter::new(Limits::default(), clock.clone()).persisted(path.clone());
        let registry = Registry::standard().with_limiter(limiter);
        let (ctx, signal) = ctx();
        ctx.store
            .insert(&incoming(2, 1, "hey"))
            .await
            .expect("seed");
        let dm = |n: u128| json!({"recipient": sid(n).to_string(), "message": "hi"});

        let result = registry.call(&ctx, "send_message", dm(2)).await;
        let Some(e) = refused(&result) else {
            panic!("a send right after a damaged limits file went through: {result:?}");
        };
        let wait = e.retry_after.as_millis() as u64;
        clock.advance(wait - 1);
        let result = registry.call(&ctx, "send_message", dm(2)).await;
        assert!(
            refused(&result).is_some(),
            "sent before the retry time: {result:?}"
        );
        clock.advance(1);
        let result = registry.call(&ctx, "send_message", dm(2)).await;
        assert!(
            result.is_ok(),
            "known conversation still refused at the retry time: {result:?}"
        );

        let result = registry.call(&ctx, "send_message", dm(3)).await;
        assert!(
            refused(&result).is_some(),
            "a first contact went through: {result:?}"
        );
        clock.advance(DAY_MS);
        let result = registry.call(&ctx, "send_message", dm(3)).await;
        assert!(
            result.is_ok(),
            "first contacts still refused a day later: {result:?}"
        );
        assert_eq!(signal.sent().len(), 2, "a refused send reached Signal");

        let aside: Vec<_> = std::fs::read_dir(&dir)
            .expect("list temp dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("limits.json.corrupt-")
            })
            .collect();
        assert_eq!(aside.len(), 1, "damaged file not kept aside");
        let kept = std::fs::read(aside[0].path()).expect("read kept file");
        assert_eq!(kept, b"{\"minute\": tru", "the damaged file was altered");
        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }

    /// Setting a damaged file aside must not leave a gap where no limits file
    /// exists, or a restart before the next send would load a fresh budget.
    #[tokio::test]
    async fn a_restart_right_after_a_damaged_limits_file_still_starts_exhausted() {
        let dir = std::env::temp_dir().join(format!("relay-limits-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("limits.json");
        std::fs::write(&path, b"garbage").expect("write damaged file");
        let clock = Arc::new(FakeClock::default());
        let start = || Limiter::new(Limits::default(), clock.clone()).persisted(path.clone());
        drop(start());
        let registry = Registry::standard().with_limiter(start());
        let (ctx, signal) = ctx();
        ctx.store
            .insert(&incoming(2, 1, "hey"))
            .await
            .expect("seed");
        let input = json!({"recipient": sid(2).to_string(), "message": "hi"});
        let result = registry.call(&ctx, "send_message", input).await;
        assert!(
            refused(&result).is_some(),
            "a restart after a damaged limits file got a fresh budget: {result:?}"
        );
        assert!(signal.sent().is_empty(), "a refused send reached Signal");
        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }

    /// Deleting limits.json while a damaged copy sits beside it must not buy a
    /// fresh budget either.
    #[tokio::test]
    async fn a_missing_limits_file_beside_a_set_aside_damaged_one_starts_exhausted() {
        let dir = std::env::temp_dir().join(format!("relay-limits-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("limits.json");
        std::fs::write(dir.join("limits.json.corrupt-1"), b"garbage").expect("write aside");
        let limiter =
            Limiter::new(Limits::default(), Arc::new(FakeClock::default())).persisted(path.clone());
        let registry = Registry::standard().with_limiter(limiter);
        let (ctx, signal) = ctx();
        ctx.store
            .insert(&incoming(2, 1, "hey"))
            .await
            .expect("seed");
        let input = json!({"recipient": sid(2).to_string(), "message": "hi"});
        let result = registry.call(&ctx, "send_message", input).await;
        assert!(
            refused(&result).is_some(),
            "a missing limits file beside a damaged one got a fresh budget: {result:?}"
        );
        assert!(signal.sent().is_empty(), "a refused send reached Signal");
        assert!(path.exists(), "the exhausted state wasn't written back");
        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }

    /// Restarting the agent must not hand out a fresh day of first contacts.
    #[tokio::test]
    async fn the_new_recipient_cap_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("relay-limits-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("limits.json");
        let clock = Arc::new(FakeClock::default());
        let limits = Limits {
            new_recipients_per_day: 1,
            ..Limits::default()
        };
        let start = || Limiter::new(limits, clock.clone()).persisted(path.clone());
        let (ctx, signal) = ctx();
        let dm = |n: u128| json!({"recipient": sid(n).to_string(), "message": "hi"});
        let registry = Registry::standard().with_limiter(start());
        let result = registry.call(&ctx, "send_message", dm(10)).await;
        assert!(result.is_ok(), "first new recipient refused: {result:?}");
        let registry = Registry::standard().with_limiter(start());
        let result = registry.call(&ctx, "send_message", dm(11)).await;
        assert!(
            refused(&result).is_some(),
            "a restart reset the new-recipient cap: {result:?}"
        );
        assert_eq!(
            signal.sent().len(),
            1,
            "a refused first contact reached Signal"
        );
        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }

    /// A settings file that names only some limits must keep the conservative
    /// defaults for the rest, and must not be able to switch a rate off.
    #[test]
    fn partial_limits_keep_defaults_and_a_zero_rate_is_refused() {
        let limits: Limits = serde_json::from_value(json!({"per_hour": 100})).expect("valid");
        assert_eq!(
            limits,
            Limits {
                per_hour: NonZeroU32::new(100).expect("nonzero literal"),
                ..Limits::default()
            }
        );
        for bad in [
            json!({"burst_per_minute": 0}),
            json!({"per_hour": 0}),
            json!({"per_houre": 5}),
        ] {
            let parsed = serde_json::from_value::<Limits>(bad.clone());
            assert!(parsed.is_err(), "accepted {bad}: {parsed:?}");
        }
    }

    struct CountingLinker(AtomicU64);

    impl CountingLinker {
        fn link(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A second link or registration inside the backoff window must be refused
    /// before the provisioner is called, so Signal never sees it. The window
    /// lives in a file so a restart doesn't reopen it, and it restarts at every
    /// allowed attempt.
    #[test]
    fn a_second_provisioning_attempt_inside_the_backoff_is_refused_without_contacting_signal() {
        let dir = std::env::temp_dir().join(format!("relay-limits-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("provisioning.json");
        let limits = Limits::default();
        let signal = CountingLinker(AtomicU64::new(0));
        // The setup driver's call site: check the gate, then provision.
        let attempt = |now_ms: u64| -> anyhow::Result<()> {
            begin_provisioning(&path, &limits, now_ms)?;
            signal.link();
            Ok(())
        };
        let start = 1_700_000_000_000;
        let backoff = 5 * MINUTE_MS;

        attempt(start).expect("first attempt allowed");
        let refusal = attempt(start + backoff - 1_000).expect_err("attempt inside backoff allowed");
        let Some(e) = refusal.downcast_ref::<LimitExceeded>() else {
            panic!("refusal is not a LimitExceeded: {refusal:#}");
        };
        assert_eq!(e.retry_after, Duration::from_secs(1), "{e}");
        assert_eq!(
            signal.0.load(Ordering::SeqCst),
            1,
            "refused attempt reached Signal"
        );

        attempt(start + backoff).expect("attempt after the backoff refused");
        let refusal = attempt(start + backoff + 1);
        assert!(
            refusal.is_err(),
            "the window did not restart at the last attempt"
        );
        assert_eq!(
            signal.0.load(Ordering::SeqCst),
            2,
            "refused attempt reached Signal"
        );
        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }

    /// Sends counted by Signal-visible message, per window of `window_ms`,
    /// must never exceed what a token bucket of `capacity` allows: the
    /// capacity plus the refill over the span between first and last.
    fn assert_within(times: &[u64], capacity: u32, window_ms: u64) -> Result<(), TestCaseError> {
        let capacity = u64::from(capacity);
        for (i, start) in times.iter().enumerate() {
            for (j, end) in times.iter().enumerate().skip(i) {
                let sent = (j - i + 1) as u64;
                prop_assert!(
                    sent * window_ms <= capacity * window_ms + capacity * (end - start),
                    "{sent} messages between {start} and {end} exceed {capacity} per {window_ms}ms"
                );
            }
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// No interleaving of write tools and pauses may push messages past
        /// either configured rate, and every refusal must leave Signal untouched.
        #[test]
        fn no_interleaving_of_write_tools_exceeds_the_configured_rate(
            per_minute in 1u32..30,
            per_hour in 1u32..300,
            calls in prop::collection::vec((0u64..8_000, 0usize..4), 1..500),
        ) {
            let limits = Limits {
                burst_per_minute: NonZeroU32::new(per_minute).expect("generated nonzero"),
                per_hour: NonZeroU32::new(per_hour).expect("generated nonzero"),
                ..Limits::default()
            };
            let runtime = tokio::runtime::Builder::new_current_thread().build()?;
            let times = runtime.block_on(async {
                let (registry, clock) = limited(limits);
                let (ctx, signal) = ctx();
                let mut times = vec![];
                for (pause, tool) in calls {
                    clock.advance(pause);
                    let (name, input) = match tool {
                        0 => ("send_message", json!({"recipient": sid(2).to_string(), "message": "hi"})),
                        1 => ("send_group_message", json!({"group_id": GROUP, "message": "hi"})),
                        2 => ("send_note_to_self", json!({"message": "hi"})),
                        _ => ("set_typing", json!({"recipient": sid(2).to_string()})),
                    };
                    let before = signal.sent().len();
                    let result = registry.call(&ctx, name, input).await;
                    let sent = signal.sent();
                    if refused(&result).is_some() {
                        assert_eq!(sent.len(), before, "{name} was refused but reached Signal");
                    }
                    if sent.len() > before && counts(&sent[before].1) {
                        times.push(clock.now_ms());
                    }
                }
                times
            });
            assert_within(&times, per_minute, MINUTE_MS)?;
            assert_within(&times, per_hour, HOUR_MS)?;
        }
    }
}
