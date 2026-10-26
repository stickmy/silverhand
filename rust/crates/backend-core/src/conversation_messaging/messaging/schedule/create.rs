//! Product CREATE for an agent's own schedule: the pure time-policy
//! canonicalization and stateless guards that turn a validated create request
//! into a canonical [`ScheduleSpec`] + initial `next_fire_at`, plus the narrow
//! atomic-insert port that enforces the active-count cap WITHOUT a
//! check-then-act race.
//!
//! Split along the fixed layering:
//!
//! - **This core module** owns everything that is a function of the REQUEST and
//!   the clock alone: the once/interval/cron canonicalization, the initial
//!   `next_fire_at`, and the STATELESS time-policy guards (one-shot delay
//!   window, interval floor + stale `start_at` floor, repeat-boundedness,
//!   timezone/cron validity, `until_at` sanity). It reads no persisted state, so
//!   it cannot decide the capacity cap.
//! - **The stateful preconditions** (owner agent exists, the resolved target
//!   conversation exists / is not closed / still has the owner as a participant and still grants `manage_schedules`) are replayed
//!   by the create UoW on its reserved writer. The route's earlier checks preserve
//!   refusal order but are not write credentials.
//! - **The capacity cap** (`too_many_active_schedules`) also lives in that one
//!   [`ScheduleCreateStore::create`] command. The UoW samples its policy clock only
//!   after the stateful gates and cap, canonicalizes the stateless request, and
//!   inserts without releasing the writer. Two concurrent creates at a cap of N
//!   therefore cannot both pass: the second observes the now-full count and returns
//!   [`ScheduleCreateOutcome::CapExceeded`] with no row written.

use std::future::Future;

use crate::foundation::identity::{AgentId, ConversationId};
use crate::foundation::persistence::PersistenceFailure;

use super::orchestrator::{ScheduleMessage, ScheduleTitle};
use super::read::ScheduleView;
use super::recurrence::{CronNextOccurrence, CronOutcome, CronQuery};
use super::spec::{ScheduleCaps, ScheduleSpec};

/// Wire quotas, mirroring `packages/shared/src/api/schedule.ts`. The Zod schema
/// enforces the interval floor and the positivity of the caps; these are the
/// create-time policy bounds this module re-checks (the interval floor as
/// defense-in-depth over the validated value, the delay window as the sole
/// authority since the wire does not bound `fire_at`/`start_at` distance).
pub const SCHEDULE_MIN_DELAY_SECONDS: i64 = 30;
pub const SCHEDULE_MAX_DELAY_SECONDS: i64 = 90 * 24 * 60 * 60;
pub const SCHEDULE_MIN_INTERVAL_SECONDS: i64 = 60;
pub const SCHEDULE_MAX_ACTIVE_PER_AGENT: i64 = 50;

/// Milliseconds per second, named so the epoch arithmetic reads in the same
/// units the wire speaks (`fire_at`/`start_at`/`until_at` are all epoch-ms while
/// the delay/interval quotas are seconds).
const MILLIS_PER_SECOND: i64 = 1_000;

/// The once-spec time input as it arrives, BEFORE canonicalization: exactly one
/// of `fire_at` (an absolute instant) or `delay_seconds` (relative to now). The
/// wire's `superRefine` guarantees the exclusivity (`!!fire_at === !!delay`), so
/// this typed form models the two legal shapes and the server maps the wire's
/// `Option`-pair into it; a value that set both or neither never reaches here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnceWhen {
    /// An absolute epoch-ms instant.
    FireAt(i64),
    /// Seconds from `now`; canonicalized to `now + delay * 1000`.
    DelaySeconds(i64),
}

/// The time policy as it arrives on a create request, BEFORE canonicalization.
/// `Interval`/`Cron` carry exactly the validated wire fields; `Once` carries the
/// resolved [`OnceWhen`]. Canonicalization ([`ScheduleCreateRequest::plan`])
/// turns this into a persisted [`ScheduleSpec`] plus the initial `next_fire_at`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScheduleSpecInput {
    Once(OnceWhen),
    Interval {
        /// Already `>= SCHEDULE_MIN_INTERVAL_SECONDS` per the Zod floor; re-checked.
        every_seconds: i64,
        /// The FIRST instant only; absent means `now + every_seconds*1000`.
        start_at: Option<i64>,
    },
    Cron {
        expression: String,
        timezone: String,
    },
}

impl ScheduleSpecInput {
    fn is_repeating(&self) -> bool {
        !matches!(self, ScheduleSpecInput::Once(_))
    }
}

/// A validated create request in DOMAIN vocabulary: the delivery payload as
/// value objects, the pre-canonical spec, and the two independent caps + the
/// explicit unbounded opt-in. The destination is the RESOLVED conversation id
/// (the wire address is already the resolver's verdict and never reaches here),
/// and the owner is the authenticated agent. The server builds this after Serde
/// admission + destination resolution; core owns the canonicalization from here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleCreateRequest {
    pub owner_agent_id: AgentId,
    pub target_conversation_id: ConversationId,
    pub title: ScheduleTitle,
    pub message: ScheduleMessage,
    pub spec: ScheduleSpecInput,
    pub max_occurrences: Option<i64>,
    pub until_at: Option<i64>,
    pub allow_unbounded: bool,
}

/// A stateless time-policy guard rejection, one per `ScheduleError` code the TS
/// create path can raise before it touches persisted state. Each maps to the
/// SAME product code the CLI already classifies on; the server owns the
/// human-readable reason string.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ScheduleCreateGuardError {
    #[error("fire_at must be at least 30s in the future")]
    FireAtTooSoon,
    #[error("fire_at must be at most 90 days in the future")]
    FireAtTooFar,
    #[error("start_at is too far in the past")]
    StartAtTooFarPast,
    #[error("repeat schedule must set max_occurrences, until_at, or allow_unbounded=true")]
    UnboundedRepeatRequiresExplicitLimit,
    #[error("until_at must be in the future")]
    UntilAtNotFuture,
    #[error("until_at is before the first fire time")]
    UntilAtBeforeFirstFire,
    #[error("'{0}' is not a valid IANA timezone")]
    InvalidTimezone(String),
    #[error("cron expression '{0}' is invalid")]
    InvalidCronExpression(String),
    /// The interval floor, re-checked as defense-in-depth over the Zod value.
    #[error("interval every_seconds must be >= 60")]
    IntervalBelowMinimum,
}

/// The canonical, ready-to-persist plan: the payload, the persisted spec, the
/// caps, and the computed initial `next_fire_at`. Produced by
/// [`ScheduleCreateRequest::plan`] once every stateless guard passes; consumed
/// only by the atomic insert command, which adds the id/status/timestamps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleCreatePlan {
    pub owner_agent_id: AgentId,
    pub target_conversation_id: ConversationId,
    pub title: ScheduleTitle,
    pub message: ScheduleMessage,
    pub spec: ScheduleSpec,
    pub caps: ScheduleCaps,
    pub next_fire_at: i64,
}

impl ScheduleCreateRequest {
    /// Canonicalize into a [`ScheduleCreatePlan`], applying every STATELESS
    /// time-policy guard in the SAME order and with the SAME thresholds as the
    /// TS `create`. Reads nothing but the request, `now`, and the injected cron
    /// evaluator; the capacity cap and the stateful preconditions are decided
    /// elsewhere.
    pub fn plan<C: CronNextOccurrence>(
        self,
        now: i64,
        cron: &C,
    ) -> Result<ScheduleCreatePlan, ScheduleCreateGuardError> {
        // Caps are only meaningful on a repeat; the wire `superRefine` already
        // rejected them on `once`, so by construction a `once` request carries
        // none and `ScheduleCaps::none()` is the honest value. A repeat must be
        // BOUNDED: at least one cap, or the explicit `allow_unbounded` opt-in.
        let caps = if self.spec.is_repeating() {
            if self.max_occurrences.is_none() && self.until_at.is_none() && !self.allow_unbounded {
                return Err(ScheduleCreateGuardError::UnboundedRepeatRequiresExplicitLimit);
            }
            // `new` rejects a non-positive cap; the wire `.positive()` makes that
            // unreachable from a valid request, but the value object is the
            // authority so an out-of-contract value is refused rather than stored.
            ScheduleCaps::new(self.max_occurrences, self.until_at)
                .map_err(|_| ScheduleCreateGuardError::UntilAtNotFuture)?
        } else {
            ScheduleCaps::none()
        };

        let (spec, next_fire_at) = match self.spec {
            ScheduleSpecInput::Once(when) => {
                let fire_at = match when {
                    OnceWhen::FireAt(instant) => instant,
                    OnceWhen::DelaySeconds(delay) => {
                        now.saturating_add(delay.saturating_mul(MILLIS_PER_SECOND))
                    }
                };
                assert_one_shot_delay(now, fire_at)?;
                (ScheduleSpec::Once { fire_at }, fire_at)
            }
            ScheduleSpecInput::Interval {
                every_seconds,
                start_at,
            } => {
                // Defense-in-depth over the Zod floor.
                if every_seconds < SCHEDULE_MIN_INTERVAL_SECONDS {
                    return Err(ScheduleCreateGuardError::IntervalBelowMinimum);
                }
                let start = start_at.unwrap_or_else(|| {
                    now.saturating_add(every_seconds.saturating_mul(MILLIS_PER_SECOND))
                });
                // A past `start_at` is fine — the tick treats it as overdue and
                // fires immediately, rearming from the actual instant. Guard only
                // against an unboundedly stale value that would grind the
                // catch-up loop.
                if start
                    < now.saturating_sub(
                        SCHEDULE_MAX_DELAY_SECONDS.saturating_mul(MILLIS_PER_SECOND),
                    )
                {
                    return Err(ScheduleCreateGuardError::StartAtTooFarPast);
                }
                let every = super::spec::IntervalSeconds::new(every_seconds)
                    .map_err(|_| ScheduleCreateGuardError::IntervalBelowMinimum)?;
                (ScheduleSpec::Interval { every, start_at }, start)
            }
            ScheduleSpecInput::Cron {
                expression,
                timezone,
            } => {
                // The evaluator judges BOTH validity and the next instant. An
                // invalid timezone and an invalid expression stay distinct
                // product codes; "no future occurrence" for a create is treated
                // as an invalid expression (a cron that can never fire is not a
                // schedulable policy), matching the TS parse-time rejection.
                let next = match cron.next_after(CronQuery {
                    expression: &expression,
                    timezone: &timezone,
                    after: now,
                }) {
                    CronOutcome::NextInstant(instant) => instant,
                    CronOutcome::InvalidTimezone => {
                        return Err(ScheduleCreateGuardError::InvalidTimezone(timezone));
                    }
                    CronOutcome::InvalidExpression | CronOutcome::NoFutureOccurrence => {
                        return Err(ScheduleCreateGuardError::InvalidCronExpression(expression));
                    }
                };
                (
                    ScheduleSpec::Cron(super::spec::CronSpec::new(expression, timezone)),
                    next,
                )
            }
        };

        // `until_at` sanity: it must be strictly in the future AND not before the
        // first fire, else the schedule could never fire once. Applies to any
        // spec that carries it (repeat-only by the wire refinement).
        if let Some(until) = caps.until_at() {
            if until <= now {
                return Err(ScheduleCreateGuardError::UntilAtNotFuture);
            }
            if until < next_fire_at {
                return Err(ScheduleCreateGuardError::UntilAtBeforeFirstFire);
            }
        }

        Ok(ScheduleCreatePlan {
            owner_agent_id: self.owner_agent_id,
            target_conversation_id: self.target_conversation_id,
            title: self.title,
            message: self.message,
            spec,
            caps,
            next_fire_at,
        })
    }
}

/// The one-shot delay window: `fire_at` must be at least
/// [`SCHEDULE_MIN_DELAY_SECONDS`] and at most [`SCHEDULE_MAX_DELAY_SECONDS`] in
/// the future. Mirrors `assertOneShotDelay`.
fn assert_one_shot_delay(now: i64, fire_at: i64) -> Result<(), ScheduleCreateGuardError> {
    let delta_ms = fire_at.saturating_sub(now);
    if delta_ms < SCHEDULE_MIN_DELAY_SECONDS.saturating_mul(MILLIS_PER_SECOND) {
        return Err(ScheduleCreateGuardError::FireAtTooSoon);
    }
    if delta_ms > SCHEDULE_MAX_DELAY_SECONDS.saturating_mul(MILLIS_PER_SECOND) {
        return Err(ScheduleCreateGuardError::FireAtTooFar);
    }
    Ok(())
}

/// The typed outcome of the atomic insert command. `Created` carries the
/// canonical persisted row (the SAME [`ScheduleView`] the list/cancel faces
/// render), so the create response echoes the row exactly as stored —
/// canonicalized `fire_at`, `status='pending'`, and the server-minted id and
/// timestamps included. `CapExceeded` means the write transaction OBSERVED the
/// owner already at [`SCHEDULE_MAX_ACTIVE_PER_AGENT`] active schedules and wrote
/// nothing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScheduleCreateOutcome {
    /// The created row, boxed: the `Created` payload is far larger than the empty
    /// refusal variants, so boxing keeps the enum small (no `large_enum_variant`).
    Created(Box<ScheduleView>),
    /// The owner agent no longer exists at the linearization point.
    AgentNotFound,
    /// The owner already holds [`SCHEDULE_MAX_ACTIVE_PER_AGENT`] active schedules.
    CapExceeded { active: i64 },
    /// The resolved target conversation no longer exists at insert time.
    TargetNotFound,
    /// The resolved target conversation is closed (`closed_at` set) — the hard
    /// "no more messages" barrier (archival is NOT a barrier and is not checked).
    TargetClosed,
    /// The owner is no longer a participant of the resolved target at insert time.
    ActorNotParticipant,
    /// A workspace placement no longer grants `manage_schedules` at linearization.
    CapabilityDenied,
    /// Stateful gates passed, then the stateless time policy refused the request.
    Invalid(ScheduleCreateGuardError),
}

/// The narrow atomic-create authority. Distinct from the fire-path
/// [`ScheduleStore`](super::orchestrator::ScheduleStore): create is its own
/// use-case, and folding it into the fire-path trait would couple two unrelated
/// write authorities.
///
/// The single method re-decides every mutable precondition and inserts INSIDE ONE
/// serialized write boundary. Two facts race the insert and are therefore replayed
/// under the lock, never trusted from a pre-lock read (the pitfalls "pre-lock
/// decision" rule):
///
/// - the active-count CAP: the count-cap-insert MUST NOT be a check-then-act
///   split, or two concurrent creates at a cap of N could both pass. The adapter
///   reserves the writer up front (a `BEGIN IMMEDIATE`-equivalent) so the second
///   create to obtain the transaction observes the full count and returns
///   [`ScheduleCreateOutcome::CapExceeded`] — not a lock-contention error. Lock
///   contention is resolved by the store's busy-timeout/serialization contract,
///   never surfaced as a third outcome.
/// - the TARGET facts: the resolver proved the destination existed and admitted
///   the owner microseconds earlier, but the window to the insert is real, so the
///   transaction re-reads the target and returns [`TargetNotFound`], [`TargetClosed`],
///   or [`ActorNotParticipant`] if it changed. These mirror the TS create's
///   post-resolve race guard.
///
/// [`TargetNotFound`]: ScheduleCreateOutcome::TargetNotFound
/// [`TargetClosed`]: ScheduleCreateOutcome::TargetClosed
/// [`ActorNotParticipant`]: ScheduleCreateOutcome::ActorNotParticipant
pub trait ScheduleCreateStore {
    /// One linearized create: owner → target existence → target
    /// closure → participation → active cap → stateless canonicalization → insert,
    /// all inside one reserved writer. This ordering is product-visible.
    fn create<C>(
        &self,
        request: ScheduleCreateRequest,
        cron: &C,
    ) -> impl Future<Output = Result<ScheduleCreateOutcome, PersistenceFailure>> + Send
    where
        C: CronNextOccurrence + Sync;
}

/// Post-commit signal that asks the timer driver to reconsider the nearest due
/// schedule. It is deliberately outside the create transaction: failure is
/// observable operationally but can never rewrite a committed create response.
pub trait ScheduleCreateCommittedEffect {
    fn rearm_near_timer(&self) -> impl Future<Output = Result<(), PersistenceFailure>> + Send;
}
