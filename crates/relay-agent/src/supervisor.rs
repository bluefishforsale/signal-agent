//! Deciding which per-profile agents should be running. One login item runs
//! `signal-agent agent --all-profiles`, because a macOS bundle can't carry a
//! login item per runtime profile name; this is the policy that process
//! follows, kept free of processes and clocks so it can be tested.

use std::{collections::BTreeMap, time::Duration};

use relay_core::ProfileName;

const FIRST_RETRY: Duration = Duration::from_secs(5);
const LONGEST_RETRY: Duration = Duration::from_secs(5 * 60);
/// An agent that ran this long before exiting was healthy; its next exit
/// starts the backoff over.
const HEALTHY_RUN: Duration = Duration::from_secs(10 * 60);

/// What the supervisor should do to one profile's agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupervisorAction {
    Start(ProfileName),
    Stop(ProfileName),
}

/// Which agents run, and when a stopped one may start again.
#[derive(Debug, Default)]
pub struct Supervisor {
    agents: BTreeMap<ProfileName, Agent>,
}

#[derive(Debug)]
struct Agent {
    running_since_ms: Option<u64>,
    retry_at_ms: u64,
    failures: u32,
}

impl Supervisor {
    /// The actions that bring the running agents in line with `ready`, the
    /// profiles that finished setup, at `now_ms`. Call it on a timer and after
    /// every exit; a profile added or removed is picked up on the next call.
    // NOTE: only ready profiles (setup saved their access mode) may start. A
    // profile still in setup has its Signal store open in the setup process,
    // and two processes driving one store corrupt its sessions.
    pub fn reconcile(&mut self, ready: &[ProfileName], now_ms: u64) -> Vec<SupervisorAction> {
        let mut actions = vec![];
        self.agents.retain(|profile, agent| {
            if ready.contains(profile) {
                return true;
            }
            if agent.running_since_ms.is_some() {
                actions.push(SupervisorAction::Stop(profile.clone()));
            }
            false
        });
        for profile in ready {
            let agent = self.agents.entry(profile.clone()).or_insert(Agent {
                running_since_ms: None,
                retry_at_ms: now_ms,
                failures: 0,
            });
            if agent.running_since_ms.is_none() && now_ms >= agent.retry_at_ms {
                agent.running_since_ms = Some(now_ms);
                actions.push(SupervisorAction::Start(profile.clone()));
            }
        }
        actions
    }

    /// Records that `profile`'s agent exited at `now_ms`, for any reason, and
    /// holds its restart back longer after each quick exit.
    // NOTE: every restart reconnects to Signal. Restarting a crashing agent
    // without backoff would hammer Signal's servers, which is exactly the
    // automated-looking traffic PRD goal 13 exists to prevent.
    pub fn exited(&mut self, profile: &ProfileName, now_ms: u64) {
        let Some(agent) = self.agents.get_mut(profile) else {
            return;
        };
        let Some(since) = agent.running_since_ms.take() else {
            return;
        };
        if now_ms.saturating_sub(since) >= HEALTHY_RUN.as_millis() as u64 {
            agent.failures = 0;
        }
        let wait = FIRST_RETRY
            .saturating_mul(1 << agent.failures.min(16))
            .min(LONGEST_RETRY);
        agent.failures = agent.failures.saturating_add(1);
        agent.retry_at_ms = now_ms.saturating_add(wait.as_millis() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> ProfileName {
        ProfileName::new(s).expect("valid profile name")
    }

    /// Every ready profile gets exactly one agent, a running one is never
    /// started twice, and a profile that stops being ready is stopped.
    #[test]
    fn each_ready_profile_runs_once_and_a_removed_one_stops() {
        let mut sup = Supervisor::default();
        let (personal, bot) = (name("personal"), name("bot"));
        let both = [personal.clone(), bot.clone()];
        let started = sup.reconcile(&both, 0);
        assert_eq!(
            started,
            vec![
                SupervisorAction::Start(personal.clone()),
                SupervisorAction::Start(bot.clone())
            ]
        );
        assert!(sup.reconcile(&both, 1).is_empty(), "started twice");
        let stopped = sup.reconcile(std::slice::from_ref(&personal), 2);
        assert_eq!(stopped, vec![SupervisorAction::Stop(bot.clone())]);
        assert!(sup.reconcile(&[personal], 3).is_empty());
    }

    /// A crashing agent must come back, but each quick exit doubles the wait
    /// up to five minutes, so a crash loop can't hammer Signal.
    #[test]
    fn a_crash_loop_backs_off_up_to_five_minutes() {
        let mut sup = Supervisor::default();
        let personal = name("personal");
        let ready = [personal.clone()];
        let mut now = 0;
        sup.reconcile(&ready, now);
        let mut waits = vec![];
        for _ in 0..10 {
            now += 1_000;
            sup.exited(&personal, now);
            let exited_at = now;
            while sup.reconcile(&ready, now).is_empty() {
                now += 1_000;
            }
            waits.push((now - exited_at) / 1_000);
        }
        assert_eq!(waits, [5, 10, 20, 40, 80, 160, 300, 300, 300, 300]);
    }

    /// An agent that ran healthily for a while and then exited restarts on
    /// the short wait again, not on a backoff left over from old crashes.
    #[test]
    fn a_healthy_run_resets_the_backoff() {
        let mut sup = Supervisor::default();
        let personal = name("personal");
        let ready = [personal.clone()];
        sup.reconcile(&ready, 0);
        for t in [1_000, 7_000, 18_000] {
            sup.exited(&personal, t);
            sup.reconcile(&ready, t + 60_000);
        }
        let later = 60_000 + HEALTHY_RUN.as_millis() as u64 * 2;
        sup.exited(&personal, later);
        assert!(sup.reconcile(&ready, later + 4_999).is_empty());
        assert_eq!(
            sup.reconcile(&ready, later + 5_000),
            vec![SupervisorAction::Start(personal)]
        );
    }
}
