//! Atomic system-message write for one claimed schedule occurrence.

use crate::conversation_messaging::runtime_delivery::conversation_allows_delivery;
use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageId, MessageSeq,
        environment::{SendClock, SendEntropy},
        schedule::{Advancement, ScheduleSpec, WakeContext},
    },
    foundation::{
        identity::{AgentId, ConversationId},
        persistence::PersistenceFailure,
    },
};
use sqlx::Row;
use thiserror::Error;

use super::super::{MessagingStore, bump_stream_head_on};

const BEGIN: &str = "BEGIN IMMEDIATE";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleWakePlacement {
    Workspace,
    DirectPair,
}

#[derive(Clone, Debug)]
pub struct CommittedScheduleWake {
    pub conversation: ConversationId,
    pub owner: AgentId,
    pub placement: ScheduleWakePlacement,
    pub message_id: MessageId,
    pub message: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScheduleWakeTargetUnavailable {
    Missing { target: ConversationId },
    Closed { target: ConversationId },
    OwnerNotParticipant { target: ConversationId },
}

impl fmt::Display for ScheduleWakeTargetUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (target, reason) = match self {
            Self::Missing { target } => (target, "missing"),
            Self::Closed { target } => (target, "closed"),
            Self::OwnerNotParticipant { target } => (target, "owner not participant"),
        };
        write!(
            formatter,
            "target_conversation_unavailable: {} ({reason})",
            target.as_str()
        )
    }
}

impl std::error::Error for ScheduleWakeTargetUnavailable {}

#[derive(Debug, Error)]
pub enum ScheduleWakeWriteError {
    #[error(transparent)]
    TargetUnavailable(#[from] ScheduleWakeTargetUnavailable),
    #[error(transparent)]
    Persistence(#[from] PersistenceFailure),
}

fn failure(error: impl std::error::Error + Send + Sync + 'static) -> ScheduleWakeWriteError {
    ScheduleWakeWriteError::Persistence(PersistenceFailure::from_source(error))
}

#[derive(Clone)]
pub struct SqliteScheduleWakeWrite<E> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}

impl<E: Clone> MessagingStore<E> {
    pub fn schedule_wake_write(&self) -> SqliteScheduleWakeWrite<E> {
        SqliteScheduleWakeWrite {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }
}

fn spec_kind(spec: &ScheduleSpec) -> &'static str {
    match spec {
        ScheduleSpec::Once { .. } => "once",
        ScheduleSpec::Interval { .. } => "interval",
        ScheduleSpec::Cron(_) => "cron",
    }
}

fn spec_summary(spec: &ScheduleSpec) -> String {
    match spec {
        ScheduleSpec::Once { .. } => "once".to_owned(),
        ScheduleSpec::Interval { every, .. } => format!("interval {}s", every.get()),
        ScheduleSpec::Cron(cron) => {
            format!("cron {} ({})", cron.expression(), cron.timezone())
        }
    }
}

fn missed_and_next(advancement: &Advancement) -> (i64, Option<i64>, bool) {
    match advancement {
        Advancement::Terminal { missed_count } => (*missed_count, None, true),
        Advancement::Rearm {
            next_fire_at,
            missed_count,
        } => (*missed_count, Some(*next_fire_at), false),
    }
}

fn relative(delta: i64) -> String {
    if delta <= 0 {
        return "on time".to_owned();
    }
    let seconds = delta.saturating_add(500) / 1_000;
    if seconds < 60 {
        return format!("fired {seconds}s late");
    }
    let minutes = seconds.saturating_add(30) / 60;
    if minutes < 60 {
        return format!("fired {minutes}m late");
    }
    let hours = minutes.saturating_add(30) / 60;
    if hours < 24 {
        return format!("fired {hours}h late");
    }
    let days = hours.saturating_add(12) / 24;
    format!("fired {days}d late")
}

fn render_body(ctx: WakeContext<'_>) -> String {
    let (missed, next, terminal) = missed_and_next(ctx.advancement);
    let planned = DateTime::<Utc>::from_timestamp_millis(ctx.claimed.scheduled_fire_at)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| ctx.claimed.scheduled_fire_at.to_string());
    let mut lines = vec![
        format!(
            "Schedule fired (kind: {}, planned {planned}, {}):",
            spec_kind(&ctx.claimed.spec),
            relative(
                ctx.actual_fire_at
                    .saturating_sub(ctx.claimed.scheduled_fire_at)
            ),
        ),
        ctx.claimed.delivery.message.as_str().to_owned(),
    ];
    if missed > 0 {
        lines.push(String::new());
        lines.push(format!("Missed {missed} prior occurrence(s) while offline; this is a single catch-up — not a backfill."));
    }
    if ctx.claimed.spec.is_repeating() {
        lines.push(String::new());
        if terminal {
            lines.push("This was the final fire of the schedule.".to_owned());
        } else if let Some(next) = next {
            let next = DateTime::<Utc>::from_timestamp_millis(next)
                .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
                .unwrap_or_else(|| next.to_string());
            let occurrence = ctx.claimed.fired_count.get().saturating_add(1);
            let cap = ctx.claimed.caps.max_occurrences().map_or_else(
                || format!("{occurrence}/∞"),
                |cap| format!("{occurrence}/{cap}"),
            );
            lines.push(format!("Next fire at {next} (fired_count: {cap})."));
        }
    }
    lines.push(String::new());
    lines.push("This is a self-scheduled task. Decide if action is needed and proceed.".to_owned());
    lines.push("If nothing to do, reply briefly with \"noted, no action\" and stop.".to_owned());
    lines.join("\n")
}

impl<E> SqliteScheduleWakeWrite<E>
where
    E: SendEntropy + SendClock + Clone + Send + Sync,
{
    pub async fn write(
        &self,
        ctx: WakeContext<'_>,
    ) -> Result<CommittedScheduleWake, ScheduleWakeWriteError> {
        let message_id = MessageId::mint(&self.environment).map_err(failure)?;
        let created_at = self.environment.now_unix_ms().map_err(failure)?;
        let conversation = &ctx.claimed.delivery.target_conversation_id;
        let owner = &ctx.claimed.delivery.owner_agent_id;
        let mut tx = self.pool.begin_with(BEGIN).await.map_err(failure)?;
        let fact = sqlx::query(
            "SELECT placement_kind,closed_at,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,endpoint_a_deleted_at,endpoint_b_deleted_at \
             FROM conversations WHERE id=?",
        )
        .bind(conversation.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(failure)?;
        let Some(fact) = fact else {
            tx.rollback().await.map_err(failure)?;
            return Err(ScheduleWakeTargetUnavailable::Missing {
                target: conversation.clone(),
            }
            .into());
        };
        if fact
            .try_get::<Option<i64>, _>("closed_at")
            .map_err(failure)?
            .is_some()
        {
            tx.rollback().await.map_err(failure)?;
            return Err(ScheduleWakeTargetUnavailable::Closed {
                target: conversation.clone(),
            }
            .into());
        }
        let placement: String = fact.try_get("placement_kind").map_err(failure)?;
        let participates = if placement == "workspace" {
            sqlx::query_scalar::<_, i64>(
                "SELECT EXISTS(SELECT 1 FROM conversation_agents WHERE conversation_id=? AND agent_id=?)",
            )
            .bind(conversation.as_str())
            .bind(owner.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(failure)? != 0
        } else {
            let a_kind: Option<String> = fact.try_get("endpoint_a_kind").map_err(failure)?;
            let a_id: Option<String> = fact.try_get("endpoint_a_id").map_err(failure)?;
            let b_kind: Option<String> = fact.try_get("endpoint_b_kind").map_err(failure)?;
            let b_id: Option<String> = fact.try_get("endpoint_b_id").map_err(failure)?;
            let a_deleted: Option<i64> = fact.try_get("endpoint_a_deleted_at").map_err(failure)?;
            let b_deleted: Option<i64> = fact.try_get("endpoint_b_deleted_at").map_err(failure)?;
            (a_kind.as_deref() == Some("agent")
                && a_id.as_deref() == Some(owner.as_str())
                && a_deleted.is_none())
                || (b_kind.as_deref() == Some("agent")
                    && b_id.as_deref() == Some(owner.as_str())
                    && b_deleted.is_none())
        };
        if !participates {
            tx.rollback().await.map_err(failure)?;
            return Err(ScheduleWakeTargetUnavailable::OwnerNotParticipant {
                target: conversation.clone(),
            }
            .into());
        }

        sqlx::query("UPDATE conversations SET latest_message_seq=latest_message_seq+1 WHERE id=?")
            .bind(conversation.as_str())
            .execute(&mut *tx)
            .await
            .map_err(failure)?;
        let seq: i64 =
            sqlx::query_scalar("SELECT latest_message_seq FROM conversations WHERE id=?")
                .bind(conversation.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(failure)?;
        let (missed_count, next_fire_at, _) = missed_and_next(ctx.advancement);
        let metadata = json!({
            "subtype": "schedule_fire",
            "schedule_id": ctx.claimed.id.as_str(),
            "title": ctx.claimed.delivery.title.as_str(),
            "spec_kind": spec_kind(&ctx.claimed.spec),
            "spec_summary": spec_summary(&ctx.claimed.spec),
            "body": ctx.claimed.delivery.message.as_str(),
            "actual_fire_at": ctx.actual_fire_at,
            "planned_fire_at": ctx.claimed.scheduled_fire_at,
            "missed_count": missed_count,
            "next_fire_at": next_fire_at,
        });
        let content = render_body(ctx);
        sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,content,metadata,seq,created_at) VALUES(?,?,'system',?,?,?,?)")
            .bind(message_id.as_str())
            .bind(conversation.as_str())
            .bind(&content)
            .bind(metadata.to_string())
            .bind(seq)
            .bind(created_at)
            .execute(&mut *tx)
            .await
            .map_err(failure)?;
        // The head describes persisted messages, not the wake's delivery audience.
        // Owner-only routing stays in the inbox write below.
        let message_seq = MessageSeq::try_from(seq).map_err(failure)?;
        bump_stream_head_on(&mut tx, conversation, None, message_seq)
            .await
            .map_err(failure)?;
        if conversation_allows_delivery(&mut tx, conversation)
            .await
            .map_err(failure)?
        {
            sqlx::query("INSERT INTO conversation_inboxes(conversation_id,agent_id,message_id,delivered_at,delivery_kind,created_at) VALUES(?,?,?,NULL,'turn',?)")
            .bind(conversation.as_str())
            .bind(owner.as_str())
            .bind(message_id.as_str())
            .bind(created_at)
            .execute(&mut *tx)
            .await
            .map_err(failure)?;
        }
        tx.commit().await.map_err(failure)?;

        Ok(CommittedScheduleWake {
            conversation: conversation.clone(),
            owner: owner.clone(),
            message_id: message_id.clone(),
            placement: if placement == "workspace" {
                ScheduleWakePlacement::Workspace
            } else {
                ScheduleWakePlacement::DirectPair
            },
            message: json!({
                "id": message_id.as_str(),
                "conversation_id": conversation.as_str(),
                "from_kind": "system",
                "from_agent_id": Value::Null,
                "from_user_id": Value::Null,
                "author": Value::Null,
                "content": content,
                "metadata": metadata,
                "attachments": Value::Null,
                "reactions": Value::Null,
                "seq": seq,
                "created_at": created_at,
                "thread_root_id": Value::Null,
                "linked_task": Value::Null,
            }),
        })
    }

    /// Best-effort operator notice for the three target-dead wake failures. The
    /// original wake failure remains the settlement result; this independent
    /// transaction only records a notice in the owner's current home DM when one
    /// exists.
    pub async fn write_dead_target_notice(
        &self,
        ctx: WakeContext<'_>,
        reason: &ScheduleWakeTargetUnavailable,
    ) -> Result<Option<CommittedScheduleWake>, PersistenceFailure> {
        let message_id =
            MessageId::mint(&self.environment).map_err(PersistenceFailure::from_source)?;
        let created_at = self
            .environment
            .now_unix_ms()
            .map_err(PersistenceFailure::from_source)?;
        let owner = &ctx.claimed.delivery.owner_agent_id;
        let mut tx = self
            .pool
            .begin_with(BEGIN)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let home: Option<String> = sqlx::query_scalar(
            "SELECT c.id FROM conversations c \
             JOIN workspaces w ON w.id=c.workspace_id \
             LEFT JOIN agents owner ON owner.id=w.owner_agent_id \
             WHERE w.owner_agent_id=? AND c.roster_policy='fixed' \
             ORDER BY CASE WHEN c.title='Onboarding' THEN 0 \
                           WHEN owner.id IS NOT NULL AND c.title='dm-'||owner.name THEN 1 \
                           ELSE 2 END, c.created_at ASC LIMIT 1",
        )
        .bind(owner.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;
        let Some(raw_home) = home else {
            tx.rollback()
                .await
                .map_err(PersistenceFailure::from_source)?;
            return Ok(None);
        };
        let home = ConversationId::parse(raw_home).map_err(PersistenceFailure::from_source)?;
        sqlx::query("UPDATE conversations SET latest_message_seq=latest_message_seq+1 WHERE id=?")
            .bind(home.as_str())
            .execute(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let seq: i64 =
            sqlx::query_scalar("SELECT latest_message_seq FROM conversations WHERE id=?")
                .bind(home.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(PersistenceFailure::from_source)?;
        let message_seq = MessageSeq::try_from(seq).map_err(PersistenceFailure::from_source)?;
        bump_stream_head_on(&mut tx, &home, None, message_seq)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let reason_text = match reason {
            ScheduleWakeTargetUnavailable::Missing { .. } => "no longer exists",
            ScheduleWakeTargetUnavailable::Closed { .. } => "has been closed",
            ScheduleWakeTargetUnavailable::OwnerNotParticipant { .. } => {
                "no longer has you as a participant"
            }
        };
        let label = if ctx.claimed.delivery.title.as_str().is_empty() {
            ctx.claimed.id.as_str()
        } else {
            ctx.claimed.delivery.title.as_str()
        };
        let content = format!(
            "Schedule \"{label}\" could not fire: target conversation {} {reason_text}. Marking the schedule as failed; create a new one with a valid target if you still need it.",
            ctx.claimed.delivery.target_conversation_id.as_str()
        );
        sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,content,seq,created_at) VALUES(?,?,'system',?,?,?)")
            .bind(message_id.as_str())
            .bind(home.as_str())
            .bind(&content)
            .bind(seq)
            .bind(created_at)
            .execute(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        if conversation_allows_delivery(&mut tx, &home)
            .await
            .map_err(PersistenceFailure::from_source)?
        {
            sqlx::query("INSERT INTO conversation_inboxes(conversation_id,agent_id,message_id,delivered_at,delivery_kind,created_at) VALUES(?,?,?,NULL,'turn',?) ON CONFLICT(conversation_id,agent_id,message_id) DO NOTHING")
            .bind(home.as_str())
            .bind(owner.as_str())
            .bind(message_id.as_str())
            .bind(created_at)
            .execute(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        }
        tx.commit().await.map_err(PersistenceFailure::from_source)?;
        Ok(Some(CommittedScheduleWake {
            conversation: home.clone(),
            owner: owner.clone(),
            placement: ScheduleWakePlacement::Workspace,
            message_id: message_id.clone(),
            message: json!({
                "id": message_id.as_str(), "conversation_id": home.as_str(),
                "from_kind":"system", "from_agent_id":Value::Null,
                "from_user_id":Value::Null, "author":Value::Null,
                "content":content, "metadata":Value::Null,
                "attachments":Value::Null, "reactions":Value::Null,
                "seq":seq, "created_at":created_at,
                "thread_root_id":Value::Null, "linked_task":Value::Null,
            }),
        }))
    }
}
