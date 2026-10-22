//! Messages saved to send later. A due job fires through the registry's own
//! send tools, so read-only mode, the input rules and the send limiter judge
//! it at fire time exactly as they would a live send.

use anyhow::Context;
use async_trait::async_trait;
use relay_core::prelude::*;
use relay_store::{JobId, ScheduledSend};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Registry, Tool, ToolContext, ToolError,
    input::{Instant, non_blank, parse, recipient, recipient_schema},
};

pub struct ScheduleMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleMessageInput {
    recipient: String,
    message: String,
    send_at: Instant,
}

#[async_trait]
impl Tool for ScheduleMessage {
    fn name(&self) -> &'static str {
        "schedule_message"
    }
    fn description(&self) -> &'static str {
        "Save a text message to send to one contact or group later. recipient is the \
         conversation (as list_conversations names it, group ids included); message is the \
         text; send_at is when, in UTC. A time already past sends on the next run. The agent \
         sends due messages about every 30 seconds while it runs, and run_scheduled_messages \
         sends them now. At send time it's an ordinary send: the profile's send limits apply, \
         and a refused or failed send is never retried but kept as failed for \
         list_scheduled_messages to show. Returns {job_id, send_at_ms}."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": recipient_schema(),
                "message": {"type": "string", "minLength": 1, "description": "Message text to send"},
                "send_at": Instant::schema("When to send"),
            },
            "required": ["recipient", "message", "send_at"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: ScheduleMessageInput = parse(input)?;
        let body = non_blank("message", input.message)?;
        let send = ScheduledSend {
            recipient: recipient(ctx, &input.recipient).await?,
            body,
            send_at_ms: input.send_at.ms(),
        };
        let id = ctx.store.schedule(&send).await?;
        Ok(json!({ "job_id": id.0, "send_at_ms": send.send_at_ms }))
    }
}

pub struct ListScheduledMessages;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListScheduledMessagesInput {}

#[async_trait]
impl Tool for ListScheduledMessages {
    fn name(&self) -> &'static str {
        "list_scheduled_messages"
    }
    fn description(&self) -> &'static str {
        "List scheduled messages not yet sent, soonest first. Each job has its id (for \
         cancel_scheduled_message), what it sends (recipient, body, send_at_ms) and its state: \
         pending; sending (claimed by a send that didn't finish, never retried); or failed, \
         with the error. Sent jobs are removed."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let ListScheduledMessagesInput {} = parse(input)?;
        let jobs = ctx.store.scheduled().await?;
        Ok(json!({ "jobs": jobs }))
    }
}

/// Sends every scheduled message due at `now_ms` through `registry`'s send
/// tools, once each, and records each outcome. Returns those outcomes.
// NOTE: never retries. A send the limiter refused must not queue up to burst
// later (PRD goal 13), and one that errored may still have reached Signal, so
// a failure is recorded and left for the user to see.
pub async fn run_due(
    registry: &Registry,
    ctx: &ToolContext,
    now_ms: u64,
) -> anyhow::Result<Vec<(JobId, Result<(), String>)>> {
    let mut fired = vec![];
    for job in ctx.store.take_due(now_ms).await? {
        let ScheduledSend {
            recipient, body, ..
        } = job.send;
        let (tool, input) = match recipient {
            Recipient::Contact(id) => (
                "send_message",
                json!({"recipient": id.to_string(), "message": body}),
            ),
            Recipient::Group(id) => (
                "send_group_message",
                json!({"group_id": id.to_string(), "message": body}),
            ),
        };
        let outcome = registry
            .call(ctx, tool, input)
            .await
            .map(drop)
            .map_err(|e| format!("{e:#}"));
        ctx.store
            .finish_scheduled(job.id, outcome.clone())
            .await
            .with_context(|| format!("recording how scheduled job {} went", job.id.0))?;
        fired.push((job.id, outcome));
    }
    Ok(fired)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use relay_signal::Outgoing;
    use relay_store::{JobState, ScheduledJob};
    use serde_json::json;

    use super::*;
    use crate::{Limiter, Limits, Registry, SystemClock, testing::*};

    /// A due job must reach Signal once and then be gone, however many ticks
    /// or run_scheduled_messages calls follow, or the recipient gets repeats.
    #[tokio::test]
    async fn a_scheduled_job_fires_exactly_once() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Primary);
        let registry = Registry::standard();
        registry
            .call(
                &ctx,
                "schedule_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "message": "happy birthday",
                    "send_at": "2030-01-01T09:00:00Z",
                }),
            )
            .await
            .unwrap();
        let due = 1_893_488_400_000;
        assert!(run_due(&registry, &ctx, due - 1).await.unwrap().is_empty());
        for _ in 0..3 {
            run_due(&registry, &ctx, due).await.unwrap();
        }
        assert_eq!(
            signal.sent(),
            [(
                Recipient::Contact(sid(2)),
                Outgoing::Text {
                    body: "happy birthday".into(),
                    quote: None,
                    attachments: vec![],
                }
            )]
        );
        assert!(ctx.store.scheduled().await.unwrap().is_empty());
    }

    /// Schedules "hi" to contact 2, due at 1 ms, and returns the job's id.
    async fn schedule_hi(ctx: &ToolContext) -> u64 {
        let out = Registry::standard()
            .call(
                ctx,
                "schedule_message",
                json!({
                    "recipient": sid(2).to_string(),
                    "message": "hi",
                    "send_at": "1970-01-01T00:00:00Z",
                }),
            )
            .await
            .unwrap();
        out["job_id"].as_u64().unwrap()
    }

    /// A send the limiter refuses must end Failed and stay that way: queueing
    /// it for a later tick would let a pile of refused jobs burst out together
    /// once the limit lifts (PRD goal 13).
    #[tokio::test]
    async fn a_job_the_limiter_refuses_ends_failed_and_is_never_sent_again() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Primary);
        let no_new_recipients = Limits {
            new_recipients_per_day: 0,
            ..Limits::default()
        };
        let registry = Registry::standard()
            .with_limiter(Limiter::new(no_new_recipients, Arc::new(SystemClock)));
        let id = schedule_hi(&ctx).await;
        let fired = run_due(&registry, &ctx, 1).await.unwrap();
        assert!(
            matches!(fired.as_slice(), [(JobId(j), Err(_))] if *j == id),
            "{fired:?}"
        );
        for later in [2, 86_400_000 * 2] {
            let fired = run_due(&registry, &ctx, later).await.unwrap();
            assert!(fired.is_empty(), "refused job fired again: {fired:?}");
        }
        assert!(signal.sent().is_empty(), "sent {:?}", signal.sent());
        let jobs = ctx.store.scheduled().await.unwrap();
        assert!(
            matches!(
                jobs.as_slice(),
                [ScheduledJob {
                    state: JobState::Failed { .. },
                    ..
                }]
            ),
            "{jobs:?}"
        );
    }

    /// Read-only mode must hold at fire time too: a job saved while the
    /// profile had full access must not send after it was switched to read-only.
    #[tokio::test]
    async fn read_only_mode_refuses_a_scheduled_job_at_fire_time() {
        let (full, signal) = ctx(AccessMode::Full, ProfileKind::Primary);
        schedule_hi(&full).await;
        let read_only = ToolContext {
            access: AccessMode::ReadOnly,
            ..full.clone()
        };
        let fired = run_due(&Registry::standard(), &read_only, 1).await.unwrap();
        assert!(
            matches!(fired.as_slice(), [(_, Err(e))] if e.contains("read-only")),
            "{fired:?}"
        );
        assert!(signal.sent().is_empty(), "sent {:?}", signal.sent());
    }

    /// list_scheduled_messages is the only place a failed job surfaces, so it
    /// must show pending jobs soonest first and a failure with its reason.
    #[tokio::test]
    async fn list_scheduled_messages_shows_jobs_soonest_first_with_failure_reasons() {
        let (ctx, _) = ctx(AccessMode::Full, ProfileKind::Primary);
        let registry = Registry::standard();
        let later = registry
            .call(
                &ctx,
                "schedule_message",
                json!({
                    "recipient": sid(3).to_string(),
                    "message": "later",
                    "send_at": "2030-01-01",
                }),
            )
            .await
            .unwrap();
        let failed = schedule_hi(&ctx).await;
        let read_only = ToolContext {
            access: AccessMode::ReadOnly,
            ..ctx.clone()
        };
        run_due(&registry, &read_only, 1).await.unwrap();
        let out = registry
            .call(&read_only, "list_scheduled_messages", json!({}))
            .await
            .unwrap();
        let jobs = out["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2, "{out}");
        assert_eq!(jobs[0]["id"], json!(failed), "{out}");
        assert_eq!(jobs[0]["state"]["state"], json!("failed"), "{out}");
        assert!(
            jobs[0]["state"]["error"]
                .as_str()
                .is_some_and(|e| e.contains("read-only")),
            "{out}"
        );
        assert_eq!(jobs[1]["id"], later["job_id"], "{out}");
        assert_eq!(jobs[1]["send"]["body"], json!("later"), "{out}");
    }
}
