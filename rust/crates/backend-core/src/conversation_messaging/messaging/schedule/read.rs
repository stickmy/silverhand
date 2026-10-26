//! The product READ model for an agent's own schedules: the list projection the
//! `GET /api/agents/{id}/schedules` face returns, and the shared [`ScheduleView`]
//! that both the list and the cancel face render to the wire.
//!
//! This is the projection contract, not a driver. [`ScheduleView`] is exactly the
//! 20-field DTO shape (`packages/shared/src/api/schedule.ts::ScheduleDtoSchema`);
//! [`target_label`] is the pure label rule. The status
//! family and the `1..=200` limit are the closed query grammar the deferred
//! parse validates AFTER the route's visibility/authority floor — never before,
//! so an invalid query never becomes an existence oracle.
//!
//! Only `id`, `owner_agent_id`, `status`, and `spec` are decoded into domain
//! values (a value outside their contract is corruption the adapter reports as a
//! safe-500, all unreachable behind the schema's constraints). Every other field
//! — including `target_conversation_id`, which the schema back-fills to the empty
//! string and the create path may leave unresolved — is echoed raw, exactly as
//! the TypeScript `rowToDto` echoes it.

use std::future::Future;

use crate::foundation::identity::AgentId;
use crate::foundation::persistence::PersistenceFailure;

use super::orchestrator::ScheduleId;
use super::spec::{ScheduleSpec, ScheduleStatus};

/// The label prefix the wire uses for a target conversation, mirroring
/// `buildTargetLabel`: a `fixed`-roster board (a DM / agent home) is `@title`, an
/// `open` board is `#title`, and an unresolvable target — the LEFT JOIN found no
/// row (`title`/`roster` are `None`) because the conversation was deleted or the
/// schedule's `target_conversation_id` is the empty back-fill — is the constant
/// `"Unavailable conversation"`. Pure: the join facts in, the label out.
pub fn target_label(title: Option<&str>, roster_policy: Option<&str>) -> String {
    match (title, roster_policy) {
        (Some(title), Some("fixed")) => format!("@{title}"),
        (Some(title), Some(_)) => format!("#{title}"),
        _ => "Unavailable conversation".to_owned(),
    }
}

/// One schedule as the product read faces project it — the exact `ScheduleDto`
/// field set. The typed fields (`id`, `owner_agent_id`, `status`, `spec`) are the
/// decode boundary; the rest ride raw from the row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleView {
    pub id: ScheduleId,
    pub owner_agent_id: AgentId,
    /// Raw target id. Echoed verbatim: the schema back-fills this to `""` and the
    /// create path may store an unresolved target, both of which
    /// [`crate::foundation::identity::ConversationId`] would reject — so this is NOT parsed.
    pub target_conversation_id: String,
    /// The pre-computed [`target_label`]; the read carries the label, never the
    /// target conversation's own metadata beyond it.
    pub target_label: String,
    pub title: String,
    pub message: String,
    pub spec: ScheduleSpec,
    pub next_fire_at: i64,
    pub status: ScheduleStatus,
    pub fired_count: i64,
    pub last_fired_at: Option<i64>,
    pub max_occurrences: Option<i64>,
    pub until_at: Option<i64>,
    pub failure_reason: Option<String>,
    pub cancelled_at: Option<i64>,
    pub last_failure_reason: Option<String>,
    pub last_failure_at: Option<i64>,
    pub consecutive_failure_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The closed status family the list query accepts. `active` is sugar for
/// `pending|firing`; `all` places no status constraint. Mirrors
/// `ScheduleListStatusFilterSchema` and `expandStatusFilter`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleStatusFilter {
    Pending,
    Firing,
    Active,
    Done,
    Cancelled,
    Failed,
    All,
}

/// A `status` query value outside the closed family — the deferred parse maps it
/// to the invalid-query 400, but only AFTER the visibility/authority floor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("status filter is not a member of the schedule status family")]
pub struct InvalidStatusFilter;

impl ScheduleStatusFilter {
    /// Parse a `status` query token. Absence maps to [`ScheduleStatusFilter::All`]
    /// at the call site (an omitted filter, per `status ?? 'all'`), so this only
    /// answers for a present value.
    pub fn parse(token: &str) -> Result<Self, InvalidStatusFilter> {
        match token {
            "pending" => Ok(ScheduleStatusFilter::Pending),
            "firing" => Ok(ScheduleStatusFilter::Firing),
            "active" => Ok(ScheduleStatusFilter::Active),
            "done" => Ok(ScheduleStatusFilter::Done),
            "cancelled" => Ok(ScheduleStatusFilter::Cancelled),
            "failed" => Ok(ScheduleStatusFilter::Failed),
            "all" => Ok(ScheduleStatusFilter::All),
            _ => Err(InvalidStatusFilter),
        }
    }

    /// The concrete statuses to match, or `None` for "no status constraint"
    /// (`all`). `active` expands to `pending|firing`; every concrete value is
    /// itself. The DB never persists the literal `active`.
    pub fn expand(self) -> Option<Vec<ScheduleStatus>> {
        match self {
            ScheduleStatusFilter::All => None,
            ScheduleStatusFilter::Active => {
                Some(vec![ScheduleStatus::Pending, ScheduleStatus::Firing])
            }
            ScheduleStatusFilter::Pending => Some(vec![ScheduleStatus::Pending]),
            ScheduleStatusFilter::Firing => Some(vec![ScheduleStatus::Firing]),
            ScheduleStatusFilter::Done => Some(vec![ScheduleStatus::Done]),
            ScheduleStatusFilter::Cancelled => Some(vec![ScheduleStatus::Cancelled]),
            ScheduleStatusFilter::Failed => Some(vec![ScheduleStatus::Failed]),
        }
    }
}

/// The list result limit: an integer in `1..=200`, defaulting to 100 when the
/// query omits it. Mirrors `ScheduleListInputSchema.limit`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduleListLimit(i64);

/// A `limit` query value outside `1..=200` (or not an integer) — mapped to the
/// invalid-query 400 after the floor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("limit must be an integer in 1..=200")]
pub struct InvalidScheduleLimit;

impl ScheduleListLimit {
    pub const MIN: i64 = 1;
    pub const MAX: i64 = 200;
    /// The value used when the query omits `limit` (`limit ?? 100`).
    pub const DEFAULT: ScheduleListLimit = ScheduleListLimit(100);

    pub fn parse(value: i64) -> Result<Self, InvalidScheduleLimit> {
        if (Self::MIN..=Self::MAX).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidScheduleLimit)
        }
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// The parsed, floored list query the read port consumes. `statuses` is the
/// already-expanded family (`None` = no constraint); the owner is the route
/// agent, bound after the authority gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleListQuery {
    pub owner_agent_id: AgentId,
    pub statuses: Option<Vec<ScheduleStatus>>,
    pub limit: ScheduleListLimit,
}

/// The list read port: every schedule the owner has, filtered by the status
/// family, ordered `next_fire_at ASC, created_at ASC` and capped at the limit.
/// A read-only projection — it opens no write transaction and shares nothing
/// with the fire-path claim/settlement store.
pub trait ScheduleListReadModel {
    fn list_owned_schedules(
        &self,
        query: &ScheduleListQuery,
    ) -> impl Future<Output = Result<Vec<ScheduleView>, PersistenceFailure>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_label_prefixes_by_roster_and_falls_back_when_unresolved() {
        assert_eq!(target_label(Some("ops"), Some("fixed")), "@ops");
        assert_eq!(target_label(Some("ops"), Some("open")), "#ops");
        assert_eq!(target_label(None, None), "Unavailable conversation");
        // A half-null join (title present, roster absent) is still unresolvable.
        assert_eq!(target_label(Some("ops"), None), "Unavailable conversation");
        assert_eq!(target_label(None, Some("open")), "Unavailable conversation");
    }

    #[test]
    fn status_filter_parses_the_closed_family_and_rejects_the_rest() {
        assert_eq!(
            ScheduleStatusFilter::parse("active"),
            Ok(ScheduleStatusFilter::Active)
        );
        assert_eq!(
            ScheduleStatusFilter::parse("all"),
            Ok(ScheduleStatusFilter::All)
        );
        assert_eq!(
            ScheduleStatusFilter::parse("bogus"),
            Err(InvalidStatusFilter)
        );
        // The persisted synonym is a query value, never a stored status.
        assert_eq!(
            ScheduleStatusFilter::parse("Active"),
            Err(InvalidStatusFilter)
        );
    }

    #[test]
    fn status_filter_expands_active_to_pending_firing_and_all_to_none() {
        assert_eq!(ScheduleStatusFilter::All.expand(), None);
        assert_eq!(
            ScheduleStatusFilter::Active.expand(),
            Some(vec![ScheduleStatus::Pending, ScheduleStatus::Firing])
        );
        assert_eq!(
            ScheduleStatusFilter::Cancelled.expand(),
            Some(vec![ScheduleStatus::Cancelled])
        );
    }

    #[test]
    fn limit_admits_the_inclusive_bounds_and_rejects_outside() {
        assert_eq!(
            ScheduleListLimit::parse(1).map(ScheduleListLimit::get),
            Ok(1)
        );
        assert_eq!(
            ScheduleListLimit::parse(200).map(ScheduleListLimit::get),
            Ok(200)
        );
        assert_eq!(ScheduleListLimit::parse(0), Err(InvalidScheduleLimit));
        assert_eq!(ScheduleListLimit::parse(201), Err(InvalidScheduleLimit));
        assert_eq!(ScheduleListLimit::parse(-5), Err(InvalidScheduleLimit));
        assert_eq!(ScheduleListLimit::DEFAULT.get(), 100);
    }
}
