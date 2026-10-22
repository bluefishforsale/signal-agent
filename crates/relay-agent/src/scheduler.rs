//! Fires scheduled sends while the agent runs, so a message scheduled for 3am
//! goes out at 3am whether or not any Claude app is open then.

use std::{sync::Arc, time::Duration};

use relay_tools::{Clock, Registry, ToolContext, run_due};
use tokio::time::MissedTickBehavior;

/// How often due jobs are looked for.
const TICK: Duration = Duration::from_secs(30);

/// Fires every due job each tick until the task is dropped.
// NOTE: `registry` must be the agent's own, limiter included, and `ctx` its
// unguarded context: run_due sends through registry.call, which is where the
// limiter applies, exactly once.
pub(crate) async fn run(registry: Arc<Registry>, ctx: ToolContext, clock: Arc<dyn Clock>) {
    let mut ticks = tokio::time::interval(TICK);
    // A tick missed while the machine slept must not turn into a run of
    // back-to-back ticks on wake.
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        // Each job's outcome is recorded on the job for list_scheduled_messages.
        if let Err(e) = run_due(&registry, &ctx, clock.now_ms()).await {
            tracing::warn!("scheduled sends not fired this tick: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use relay_core::prelude::*;
    use relay_signal::RateLimited;
    use relay_store::{JobState, MemoryStore, MessageStore, ScheduledSend};
    use relay_tools::{Limiter, Limits};
    use uuid::Uuid;

    use super::*;
    use crate::tests::fake_signal;

    struct FakeClock(AtomicU64);

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn to(n: u128) -> Recipient {
        Recipient::Contact(ServiceId::new(Uuid::from_u128(n)))
    }

    /// A due job goes out exactly once however many ticks pass, and a job
    /// whose send failed is left Failed, never retried: a retry could double
    /// a send that reached Signal, or turn a limiter refusal into a burst.
    #[tokio::test(start_paused = true)]
    async fn due_jobs_fire_once_and_failures_are_never_retried() {
        let signal = fake_signal();
        let store = Arc::new(MemoryStore::default());
        let clock = Arc::new(FakeClock(AtomicU64::new(0)));
        let ctx = ToolContext {
            store: store.clone(),
            signal: signal.clone(),
            access: AccessMode::Full,
            kind: ProfileKind::Linked,
            send_roots: Arc::default(),
            webhook: None,
        };
        let failing = ScheduledSend {
            recipient: to(2),
            body: "fails".into(),
            send_at_ms: 40_000,
        };
        let later = ScheduledSend {
            recipient: to(3),
            body: "goes out".into(),
            send_at_ms: 100_000,
        };
        let failing = store.schedule(&failing).await.expect("schedule");
        store.schedule(&later).await.expect("schedule");
        signal.rate_limit_next_send(RateLimited {
            message: "slow down".into(),
            retry_after: Some(Duration::from_secs(10)),
        });
        // The limiter pauses sends for Signal's Retry-After, judged on the
        // same clock the jobs fall due by.
        let registry =
            Registry::standard().with_limiter(Limiter::new(Limits::default(), clock.clone()));
        let ticking = tokio::spawn(run(Arc::new(registry), ctx, clock.clone()));
        for _ in 0..20 {
            tokio::time::sleep(TICK).await;
            clock.0.fetch_add(TICK.as_millis() as u64, Ordering::SeqCst);
        }
        ticking.abort();
        assert_eq!(
            signal.attempts(),
            2,
            "each job must reach Signal exactly once"
        );
        let sent = signal.sent();
        assert_eq!(sent.len(), 1, "sent: {sent:?}");
        assert_eq!(sent.first().map(|(r, _)| *r), Some(to(3)));
        let jobs = store.scheduled().await.expect("scheduled");
        let failed = jobs.iter().find(|j| j.id == failing).map(|j| &j.state);
        assert!(
            matches!(failed, Some(JobState::Failed { .. })),
            "failed job left {failed:?}"
        );
    }
}
