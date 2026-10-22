//! Account-safety limits. Signal flags accounts that look automated: high send
//! rates and many first contacts. An AI client can loop, and every client goes
//! through this agent, so the brakes live here rather than in each tool.

use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures::stream::BoxStream;
use relay_core::prelude::*;
use relay_signal::{Account, Contact, Group, Incoming, Outgoing, SignalBackend};
use relay_store::MessageStore;

use crate::ToolContext;

const MINUTE_MS: u64 = 60_000;
const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;

/// How much a profile may send before tools refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Messages allowed back to back, refilling over a minute.
    pub burst_per_minute: NonZeroU32,
    /// Messages allowed back to back, refilling over an hour.
    pub per_hour: NonZeroU32,
    /// Conversations with no history that may get a first message in any
    /// 24 hours. A group is one conversation.
    pub new_recipients_per_day: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            burst_per_minute: NonZeroU32::new(20).expect("nonzero literal"),
            per_hour: NonZeroU32::new(200).expect("nonzero literal"),
            new_recipients_per_day: 20,
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
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    minute: Bucket,
    hour: Bucket,
    /// Conversations first messaged in the last day, oldest first.
    first_contacts: Vec<(Recipient, u64)>,
}

/// A token bucket in exact integer units: one token is `window_ms` units, and
/// it refills `capacity` units per millisecond, so a full bucket takes one
/// window to refill from empty.
#[derive(Default)]
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
            state: Mutex::default(),
        }
    }
}

impl Limiter {
    /// Counts one message to `to`, or refuses it.
    async fn admit(&self, store: &dyn MessageStore, to: Recipient) -> anyhow::Result<()> {
        let known = !store.history(to, None, 1).await?.is_empty();
        let now = self.clock.now_ms();
        let mut state = self.state.lock().expect("limiter state");
        let Limits {
            burst_per_minute,
            per_hour,
            new_recipients_per_day,
        } = self.limits;
        state
            .first_contacts
            .retain(|(_, at)| now.saturating_sub(*at) < DAY_MS);
        let first = !known && !state.first_contacts.iter().any(|(r, _)| *r == to);
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
        if let Some(retry_after) = state.minute.wait(now, burst_per_minute, MINUTE_MS) {
            return Err(LimitExceeded {
                limit: format!("at most {burst_per_minute} messages a minute"),
                retry_after,
            }
            .into());
        }
        if let Some(retry_after) = state.hour.wait(now, per_hour, HOUR_MS) {
            return Err(LimitExceeded {
                limit: format!("at most {per_hour} messages an hour"),
                retry_after,
            }
            .into());
        }
        state.minute.take(MINUTE_MS);
        state.hour.take(HOUR_MS);
        if first {
            state.first_contacts.push((to, now));
        }
        Ok(())
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new(Limits::default(), Arc::new(SystemClock))
    }
}

/// `ctx` with its Signal connection routed through `limiter`.
pub(crate) fn guard(limiter: &Arc<Limiter>, ctx: &ToolContext) -> ToolContext {
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
