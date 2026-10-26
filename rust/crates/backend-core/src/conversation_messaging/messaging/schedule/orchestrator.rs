//! The claim/settle unit of work: how ONE due schedule is fired.
//!
//! One layer above [`super::recurrence`] and [`super::settlement`]. Those are
//! pure decisions; THIS module is the driver-free use-case that sequences them
//! around the two real side effects — claiming the row and delivering the wake
//! — over consumer-defined ports. It owns no clock, no database, no timer, and
//! no cron parser: [`Clock`], [`ScheduleStore`], [`ScheduleWake`], and
//! [`super::recurrence::CronNextOccurrence`] are all injected. The near-wake
//! timer driver and the periodic sweep that call [`process_one`] are a separate
//! seam; product create/list/cancel never depend on them.
//!
//! The ordering IS the contract:
//!
//! 1. **Claim the EXACT occurrence first.** A single `pending → firing` CAS
//!    bound to the [`DueCandidate`]'s `(id, fired_count, next_fire_at)`, whose
//!    `RETURNING` projects the authoritative [`ClaimedSchedule`]. The candidate
//!    is a pre-claim sweep snapshot and nothing more: everything the fire plans
//!    and delivers from is read back from the row the CAS actually won, never
//!    from the candidate. Losing the claim — the row is gone, no longer pending,
//!    or already re-armed to a NEWER occurrence than the candidate named — is a
//!    clean no-op ([`ProcessOutcome::ClaimLost`]): no clock read, no delivery, no
//!    write. This is the occurrence-ABA guard: a stale candidate for occurrence
//!    N cannot win a row that a concurrent tick already fired and re-armed to
//!    occurrence N+1, so a fire can never run on stale coordinates.
//! 2. **Compute the success plan BEFORE delivering.** The wake body needs the
//!    next instant and the missed-count, so [`plan_advancement`] runs first over
//!    the CLAIMED fact's policy inputs. It is deliberately OUTSIDE the delivery
//!    attempt: a cadence fault here (a cron expression that no longer parses /
//!    has no future occurrence) escapes as [`ProcessError`] with NO settlement
//!    write, leaving the row `firing`. That is not a lost write — it is the
//!    at-least-once contract: boot recovery resets stale `firing` rows to
//!    `pending` and the next tick re-fires. The TS throw out of `processOne` has
//!    the identical effect.
//! 3. **Deliver, then settle with exactly ONE durable write.** The wake reads
//!    its target/title/message SOLELY from the claimed fact's
//!    [`DeliveryMaterial`] — there is no `ScheduleId`→payload re-read port, so
//!    the delivery cannot drift from the occurrence that was claimed. Delivery
//!    succeeds or fails; its outcome (never collapsed into the settlement
//!    result) selects the [`SettlementAction`] via the pure [`plan_settlement`].
//!    That single action is applied as one `status='firing'`-predicated CAS. A
//!    racing `cancel` that flipped the row to `cancelled` after our claim makes
//!    the CAS a no-op ([`ProcessOutcome::Settled`] with `applied: false`); the
//!    orchestrator accepts that — the wake was already delivered and cancel only
//!    promises "no FUTURE fires". Post-delivery faults never compensate the
//!    delivered wake: once delivered we settle, we never un-send.

use std::future::Future;

use crate::foundation::identity::{AgentId, ConversationId};
use crate::foundation::persistence::PersistenceFailure;

use super::recurrence::{
    AdvanceError, Advancement, CronNextOccurrence, FailureRearmError, plan_advancement,
};
use super::settlement::{DeliveryOutcome, SettlementAction, plan_settlement};
use super::spec::{FiredCount, ScheduleCaps, ScheduleSpec};

/// An opaque, persisted schedule handle.
///
/// Decoding is fallible ON PURPOSE and there is no infallible constructor: a
/// schedule id only ever originates from a stored row — the due sweep, or a
/// claim's `RETURNING` — so an empty id is database corruption, surfaced as a
/// decode failure the adapter routes to a safe-500, NEVER as a product "not
/// found". This slice never turns an untrusted request string into a
/// `ScheduleId`, so there is no "any string → lookup key" arm to conflate with a
/// trusted persisted identity; a future wire lookup that needs one uses its own
/// input type rather than reusing this.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ScheduleId(String);

/// A stored schedule id that decoded to the empty string — persistence
/// corruption, not a caller's mistake.
#[derive(Debug, thiserror::Error)]
#[error("schedule id must not be empty")]
pub struct EmptyScheduleId;

impl ScheduleId {
    pub fn parse(raw: impl Into<String>) -> Result<Self, EmptyScheduleId> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(EmptyScheduleId);
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Persisted-value ceilings for the delivery payload, mirroring the wire create
/// contract (`ScheduleDefinitionSchema` in `packages/shared/src/api/schedule.ts`:
/// title `trim().min(1).max(120)`, message `min(1).max(500)`). Create-time
/// validation lives on the wire; these are re-checked at the DECODE boundary
/// because the `schedules` columns carry no length CHECK, so a persisted row
/// that violates them is corruption, not a legal domain fact. Counted in UTF-16
/// code units — the SAME unit Zod's `.max()` counts (JS `String.length`) — so
/// the ceiling is EXACTLY the wire's, not a wider scalar-count budget that would
/// admit an astral-heavy string the wire rejected (120 astral scalars = 240
/// UTF-16 units: rejected at create, so it must be rejected on decode too).
pub const SCHEDULE_TITLE_MAX_LEN: usize = 120;
pub const SCHEDULE_MESSAGE_MAX_LEN: usize = 500;

/// A schedule's human title as a validated persisted value: within
/// [`SCHEDULE_TITLE_MAX_LEN`] UTF-16 units, non-empty, AND already trimmed.
///
/// There is no infallible constructor and no `Default`: a title only ever
/// originates from a stored row (or a test), so a value the create contract
/// could not have written is decode-time corruption the adapter routes to a
/// safe-500, NEVER a product error. The wire stores `trim()`-ed titles, so a
/// decoded title with surrounding whitespace (or all whitespace) is a row the
/// create path could not have produced — rejected, not silently re-trimmed
/// (re-trimming would mutate a persisted fact and mask the corruption).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleTitle(String);

/// A schedule's wake message as a validated persisted value: within
/// [`SCHEDULE_MESSAGE_MAX_LEN`] UTF-16 units and non-empty. Same decode-
/// corruption discipline as [`ScheduleTitle`]; unlike the title the wire never
/// trims it, so leading and trailing whitespace is legitimate content preserved
/// verbatim and is NOT a corruption signal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleMessage(String);

/// A persisted title/message that decoded outside its domain contract — empty,
/// over the UTF-16 ceiling, or (title only) not already trimmed. Persistence
/// corruption, not a caller's mistake.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum InvalidDeliveryText {
    #[error("schedule title must not be empty")]
    EmptyTitle,
    #[error("schedule title must be stored trimmed (no surrounding whitespace)")]
    TitleNotTrimmed,
    #[error("schedule title exceeds the maximum length")]
    TitleTooLong,
    #[error("schedule message must not be empty")]
    EmptyMessage,
    #[error("schedule message exceeds the maximum length")]
    MessageTooLong,
}

/// The EXACT set of code points ECMAScript `String.prototype.trim` strips:
/// WhiteSpace ∪ LineTerminator (ECMA-262 §12.2 WhiteSpace, §12.3
/// LineTerminator). WhiteSpace = TAB, VT, FF, SP, NBSP, ZWNBSP, and every
/// `Space_Separator` (Zs); LineTerminator = LF, CR, LS, PS.
///
/// This is DELIBERATELY not Rust `char::is_whitespace` (Unicode `White_Space`),
/// which disagrees at the boundary in BOTH directions and would break the
/// cross-language canonicality this decoder asserts:
///   - U+FEFF (ZWNBSP) is ES WhiteSpace but NOT Unicode `White_Space`: JS trims
///     it, so `create` can never persist FEFF padding — a decoded title with it
///     is corruption. `char::is_whitespace` keeps it and would wrongly accept.
///   - U+0085 (NEL) is Unicode `White_Space` but NOT in the ES set: JS leaves
///     it, so a TS-persisted title may legitimately carry it.
///     `char::is_whitespace` strips it and would wrongly reject a valid row.
fn is_ecmascript_trim_ws(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'                  // TAB
            | '\u{000A}'            // LF  (LineTerminator)
            | '\u{000B}'            // VT
            | '\u{000C}'            // FF
            | '\u{000D}'            // CR  (LineTerminator)
            | '\u{0020}'            // SPACE
            | '\u{00A0}'            // NBSP                         (Zs)
            | '\u{1680}'            // OGHAM SPACE MARK             (Zs)
            | '\u{2000}'
            ..='\u{200A}' // EN QUAD .. HAIR SPACE     (Zs)
            | '\u{2028}'            // LS  (LineTerminator)
            | '\u{2029}'            // PS  (LineTerminator)
            | '\u{202F}'            // NARROW NBSP                 (Zs)
            | '\u{205F}'            // MEDIUM MATHEMATICAL SPACE    (Zs)
            | '\u{3000}'            // IDEOGRAPHIC SPACE            (Zs)
            | '\u{FEFF}' // ZWNBSP / BOM
    )
}

impl ScheduleTitle {
    pub fn parse(raw: impl Into<String>) -> Result<Self, InvalidDeliveryText> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(InvalidDeliveryText::EmptyTitle);
        }
        // The wire persists `trim()`-ed titles, so a decoded value with leading
        // or trailing ECMAScript trim code points (or one that is all such code
        // points) is a row create could not have written. The trim set is the
        // EXACT ES set, not Rust's Unicode `White_Space` — see
        // [`is_ecmascript_trim_ws`].
        if raw.trim_matches(is_ecmascript_trim_ws) != raw {
            return Err(InvalidDeliveryText::TitleNotTrimmed);
        }
        if raw.encode_utf16().count() > SCHEDULE_TITLE_MAX_LEN {
            return Err(InvalidDeliveryText::TitleTooLong);
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ScheduleMessage {
    pub fn parse(raw: impl Into<String>) -> Result<Self, InvalidDeliveryText> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(InvalidDeliveryText::EmptyMessage);
        }
        if raw.encode_utf16().count() > SCHEDULE_MESSAGE_MAX_LEN {
            return Err(InvalidDeliveryText::MessageTooLong);
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The immutable delivery material of a claimed fire: everything the wake body
/// needs, as committed state of the schedule bounded context.
///
/// It is produced by the winning claim's `RETURNING` projection and travels on
/// [`ClaimedSchedule`], so the wake NEVER re-reads the schedule row by id. A
/// second, non-snapshot authority read would let the payload drift from the
/// occurrence that was actually claimed and would couple the delivery effect
/// back into the schedule store; the real wake adapter composes ONLY the
/// committed system-message application port (the #2194 send seam) and runtime
/// delivery from these fields. `title`/`message` are validated value objects,
/// not raw `String`, so a corrupt row cannot present an empty or oversized
/// payload as a legal fired occurrence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryMaterial {
    pub owner_agent_id: AgentId,
    pub target_conversation_id: ConversationId,
    pub title: ScheduleTitle,
    pub message: ScheduleMessage,
}

/// A due row as the pre-claim sweep saw it — a HYPOTHESIS about which
/// occurrence is pending, never trusted past the claim.
///
/// It carries exactly the coordinates the claim CAS binds so a stale candidate
/// cannot win a superseded occurrence: `id`, and the `(fired_count,
/// next_fire_at)` pair that identifies THIS occurrence. If another tick already
/// fired the row and re-armed it, that pair moved on, and a claim carrying the
/// old pair matches no row even though the row is `pending` again. The
/// authoritative policy inputs and delivery material come back from the claim
/// ([`ClaimedSchedule`]), not from here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DueCandidate {
    pub id: ScheduleId,
    /// The fired count this candidate expects the row to still have. It strictly
    /// increases on every settled success, so it distinguishes occurrence N from
    /// a re-armed N+1 even when `next_fire_at` happens to coincide.
    pub expected_fired_count: FiredCount,
    /// The `next_fire_at` this candidate believes is due — the tick the sweep
    /// selected the row for.
    pub expected_next_fire_at: i64,
}

/// The authoritative fact produced by a winning claim's `RETURNING`.
///
/// Every policy input ([`spec`](Self::spec), [`caps`](Self::caps),
/// [`fired_count`](Self::fired_count), [`scheduled_fire_at`](Self::scheduled_fire_at))
/// and all [`delivery`](Self::delivery) material is read from the row the CAS
/// actually flipped to `firing`, so [`process_one`] plans and delivers from the
/// occurrence it WON — never from the [`DueCandidate`] snapshot, which may
/// describe a superseded occurrence. This is the single identity of the fire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimedSchedule {
    pub id: ScheduleId,
    pub spec: ScheduleSpec,
    pub caps: ScheduleCaps,
    pub fired_count: FiredCount,
    /// The row's stored `next_fire_at`: the tick this fire is FOR. Feeds the
    /// missed-count hint (against the actual fire instant); the rearm base is
    /// the ACTUAL fire time, never this.
    pub scheduled_fire_at: i64,
    pub delivery: DeliveryMaterial,
}

/// The wall clock. A distinct port from
/// [`super::recurrence::CronNextOccurrence`] on purpose: this answers "what
/// instant is it now", that answers "when does this expression next fire after
/// T". They never merge. Reading the current instant is not IO in the
/// transactional sense, so this stays synchronous.
pub trait Clock {
    /// The current instant, epoch-ms UTC.
    fn now(&self) -> i64;
}

/// The complete claimed occurrence a wake fires from, handed to
/// [`ScheduleWake::deliver`].
///
/// It borrows the WHOLE winning [`ClaimedSchedule`] — spec, caps, fired_count,
/// the scheduled (planned) fire instant, and the delivery material — plus the
/// only two values the claim cannot carry because they are computed AFTER it:
/// the actual fire instant and the pre-delivery success plan. That is
/// deliberate: every field the wake metadata consumes is reachable from here —
/// spec kind and its human summary, the planned instant vs the actual one, the
/// missed count and the next instant (from [`advancement`](Self::advancement)),
/// the occurrence label (from `fired_count` + `caps`), and the title/message —
/// so the adapter neither re-reads the row by id (which would reopen occurrence
/// drift) nor recomputes a field it should have been handed. The wake fact is
/// the claimed occurrence itself, not a hand-picked subset that can silently
/// drop a field.
#[derive(Clone, Copy, Debug)]
pub struct WakeContext<'a> {
    pub claimed: &'a ClaimedSchedule,
    pub actual_fire_at: i64,
    pub advancement: &'a Advancement,
}

/// A wake delivery that failed. The `reason` is recorded verbatim on the
/// schedule's failure settlement (`last_failure_reason` / `failure_reason`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WakeFailed {
    pub reason: String,
}

/// Delivering the wake for a claimed fire. `Ok(())` is delivered; `Err` is a
/// fire-level failure whose reason feeds the failure settlement. This is the
/// only side effect between claim and settle, and its result is kept separate
/// from the settlement write — the two are never folded into one `Result`.
pub trait ScheduleWake {
    fn deliver(&self, ctx: WakeContext<'_>) -> impl Future<Output = Result<(), WakeFailed>> + Send;
}

/// The durable store, reduced to the two writes a fire performs. Each is a
/// single conditional CAS in the adapter; the orchestrator only observes what it
/// won and whether the settlement applied.
pub trait ScheduleStore {
    /// Claim the candidate's EXACT occurrence: a `pending → firing` CAS
    /// predicated on `id` AND the candidate's `(fired_count, next_fire_at)`,
    /// projecting the row's `RETURNING` into a [`ClaimedSchedule`].
    ///
    /// `Ok(Some(_))` means this caller won and carries the authoritative fact to
    /// fire from. `Ok(None)` means the CAS matched no row — gone, no longer
    /// pending, or already re-armed to a newer occurrence than the candidate
    /// named — a clean [`ProcessOutcome::ClaimLost`]. Binding the occurrence
    /// coordinates (not just the id) is what stops a stale candidate from
    /// winning a superseded occurrence and firing on stale coordinates.
    fn claim(
        &self,
        candidate: &DueCandidate,
    ) -> impl Future<Output = Result<Option<ClaimedSchedule>, PersistenceFailure>> + Send;

    /// Apply the settlement decision as ONE `status='firing'`-predicated write.
    /// `actual_fire_at` is used only by the arms that record a delivered fire
    /// (`last_fired_at`); the reason / next instant travel in the action.
    /// `Ok(false)` means the CAS matched no `firing` row — a racing terminal
    /// write (typically `cancel`) got there first, and the caller treats the
    /// settlement as a benign no-op.
    fn apply_settlement(
        &self,
        id: &ScheduleId,
        actual_fire_at: i64,
        action: &SettlementAction,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;
}

/// The result of a completed [`process_one`] pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessOutcome {
    /// The claim CAS was lost to a concurrent tick — nothing else happened.
    ClaimLost,
    /// The fire was claimed, delivered-or-failed, and settled with one write.
    /// `applied` is `false` when a racing cancel made the settlement a no-op.
    Settled {
        action: SettlementAction,
        applied: bool,
    },
}

/// Why a [`process_one`] pass could not settle a claimed fire.
///
/// Every variant leaves the row in `firing` with NO settlement write, which is
/// the intended at-least-once behavior: the boot-recovery sweep resets stale
/// `firing` rows to `pending` and a later tick re-fires. This mirrors the TS
/// service, where the same faults throw out of `processOne` and the row is
/// likewise left claimed for recovery.
///
/// Each variant preserves its cause as an error `source`, so the operator reason
/// (a cron cadence fault, a driver failure) survives to the boundary that logs
/// it rather than collapsing to a bare Debug discriminant. (`PersistenceFailure`
/// has no `PartialEq`, so this type is matched on, not compared.)
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// Computing the pre-delivery success plan faulted (only reachable for
    /// `cron`: a cadence fault, or an internal policy fault → safe-500). The
    /// wake was NOT delivered.
    #[error("computing the pre-delivery success plan faulted; the wake was not delivered")]
    Advance(#[from] AdvanceError),
    /// Computing the failed-fire rearm faulted after a failed delivery (a cron
    /// cadence/policy fault). The wake delivery had already failed; the row is
    /// left `firing` for recovery rather than half-settled.
    #[error("computing the failed-fire rearm faulted after a failed delivery")]
    FailureRearm(#[from] FailureRearmError),
    /// A store CAS itself failed at the infrastructure level.
    #[error("a schedule store write failed")]
    Store(#[from] PersistenceFailure),
}

/// Fire ONE due schedule: claim → plan → deliver → settle, as documented at the
/// module level. Pure control flow over the injected ports; the only decisions
/// it makes itself are the ordering and the claim-lost / cancel-race no-ops.
///
/// `candidate` is only ever the claim predicate. Once claimed, every input is
/// the authoritative [`ClaimedSchedule`] — the candidate's coordinates are never
/// read again.
pub async fn process_one<S, W, K, C>(
    store: &S,
    wake: &W,
    clock: &K,
    cron: &C,
    candidate: &DueCandidate,
) -> Result<ProcessOutcome, ProcessError>
where
    S: ScheduleStore,
    W: ScheduleWake,
    K: Clock,
    C: CronNextOccurrence,
{
    let Some(claimed) = store.claim(candidate).await? else {
        return Ok(ProcessOutcome::ClaimLost);
    };

    // Claimed. From here every early return via `?` intentionally leaves the
    // row `firing` for boot-recovery redelivery — see ProcessError. Every value
    // below is read from `claimed` (the won occurrence), never from `candidate`.
    let actual_fire_at = clock.now();

    // Pre-delivery: the wake body carries the next instant + missed count.
    let advancement = plan_advancement(
        &claimed.spec,
        claimed.caps,
        claimed.fired_count,
        claimed.scheduled_fire_at,
        actual_fire_at,
        cron,
    )?;

    let outcome = match wake
        .deliver(WakeContext {
            claimed: &claimed,
            actual_fire_at,
            advancement: &advancement,
        })
        .await
    {
        Ok(()) => DeliveryOutcome::Delivered,
        Err(WakeFailed { reason }) => DeliveryOutcome::Failed { reason },
    };

    // The delivered path reuses `advancement` and never touches cadence, so it
    // cannot error; only a failed repeating fire recomputes the rearm and may
    // surface a cadence/policy fault here.
    let action = plan_settlement(
        &claimed.spec,
        claimed.caps,
        actual_fire_at,
        &advancement,
        outcome,
        cron,
    )?;

    let applied = store
        .apply_settlement(&claimed.id, actual_fire_at, &action)
        .await?;
    Ok(ProcessOutcome::Settled { action, applied })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::recurrence::{Advancement, CronOutcome, CronQuery};
    use super::super::spec::IntervalSeconds;
    use super::*;

    const OWNER: &str = "agent-1";
    const TARGET: &str = "conv-1";
    const TITLE: &str = "daily digest";
    const MESSAGE: &str = "your 9am summary";

    /// Every port call the orchestrator made, in order — the observable proof
    /// that the sequence and the "exactly one settlement write" invariant hold.
    /// `Deliver` captures the routing + actual instant it received, so a test can
    /// prove the wake ran on the winning claim's material at the clock's instant;
    /// the COMPLETE wake metadata is captured separately in [`WakeMeta`].
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Call {
        Claim,
        Deliver {
            actual_fire_at: i64,
            owner_agent_id: String,
            target_conversation_id: String,
            title: String,
            message: String,
        },
        Settle {
            action: SettlementAction,
            actual_fire_at: i64,
        },
    }

    /// The FULL set of fields the wake metadata and body consume, extracted from a single [`WakeContext`]. Capturing every one of them and
    /// asserting the whole struct is the guard that the wake fact is the complete
    /// claimed occurrence — not a subset that silently drops `spec`, the planned
    /// instant, or the occurrence-label inputs.
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct WakeMeta {
        schedule_id: String,
        spec_kind: &'static str,
        spec_summary: String,
        planned_fire_at: i64,
        actual_fire_at: i64,
        missed_count: i64,
        next_fire_at: Option<i64>,
        fired_count: i64,
        max_occurrences: Option<i64>,
        owner_agent_id: String,
        target_conversation_id: String,
        title: String,
        message: String,
    }

    /// `metadata.spec_kind` — the discriminant TS reads as `dto.spec.kind`.
    fn spec_kind(spec: &ScheduleSpec) -> &'static str {
        match spec {
            ScheduleSpec::Once { .. } => "once",
            ScheduleSpec::Interval { .. } => "interval",
            ScheduleSpec::Cron(_) => "cron",
        }
    }

    /// `metadata.spec_summary` — byte-identical to TS `renderSpecSummary`
    /// (`once` / `interval {n}s` / `cron {expr} ({tz})`). Rendered here in the
    /// test to prove the carried `spec` is sufficient to reproduce it; the real
    /// wake adapter renders it the same way from the same carried spec.
    fn spec_summary(spec: &ScheduleSpec) -> String {
        match spec {
            ScheduleSpec::Once { .. } => "once".to_string(),
            ScheduleSpec::Interval { every, .. } => format!("interval {}s", every.get()),
            ScheduleSpec::Cron(c) => format!("cron {} ({})", c.expression(), c.timezone()),
        }
    }

    fn wake_meta_of(ctx: &WakeContext<'_>) -> WakeMeta {
        let c = ctx.claimed;
        let (missed_count, next_fire_at) = match ctx.advancement {
            Advancement::Terminal { missed_count } => (*missed_count, None),
            Advancement::Rearm {
                next_fire_at,
                missed_count,
            } => (*missed_count, Some(*next_fire_at)),
        };
        WakeMeta {
            schedule_id: c.id.as_str().to_owned(),
            spec_kind: spec_kind(&c.spec),
            spec_summary: spec_summary(&c.spec),
            planned_fire_at: c.scheduled_fire_at,
            actual_fire_at: ctx.actual_fire_at,
            missed_count,
            next_fire_at,
            fired_count: c.fired_count.get(),
            max_occurrences: c.caps.max_occurrences(),
            owner_agent_id: c.delivery.owner_agent_id.as_str().to_owned(),
            target_conversation_id: c.delivery.target_conversation_id.as_str().to_owned(),
            title: c.delivery.title.as_str().to_owned(),
            message: c.delivery.message.as_str().to_owned(),
        }
    }

    /// A driver-free store + wake double. `Mutex` (not a `RefCell`) so the
    /// port futures stay `Send` like a real adapter's; the orchestrator awaits
    /// each call to completion before the next and the fakes never suspend, so
    /// no guard is ever held across an `.await`.
    ///
    /// It has NO `ScheduleId`→payload capability at all: the only place delivery
    /// material can come from is `claim_result`, which is exactly the P0-2
    /// structural guarantee.
    struct Recorder {
        calls: Mutex<Vec<Call>>,
        wake_meta: Mutex<Option<WakeMeta>>,
        claim_result: Option<ClaimedSchedule>,
        settlement_applies: bool,
        deliver_result: Result<(), WakeFailed>,
    }

    impl Recorder {
        fn won(claimed: ClaimedSchedule) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                wake_meta: Mutex::new(None),
                claim_result: Some(claimed),
                settlement_applies: true,
                deliver_result: Ok(()),
            }
        }
        fn claim_lost() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                wake_meta: Mutex::new(None),
                claim_result: None,
                settlement_applies: true,
                deliver_result: Ok(()),
            }
        }
        fn cancel_race(mut self) -> Self {
            self.settlement_applies = false;
            self
        }
        fn delivery_fails(mut self, reason: &str) -> Self {
            self.deliver_result = Err(WakeFailed {
                reason: reason.into(),
            });
            self
        }
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
        fn wake_meta(&self) -> Option<WakeMeta> {
            self.wake_meta.lock().unwrap().clone()
        }
    }

    impl ScheduleStore for Recorder {
        async fn claim(
            &self,
            _candidate: &DueCandidate,
        ) -> Result<Option<ClaimedSchedule>, PersistenceFailure> {
            self.calls.lock().unwrap().push(Call::Claim);
            Ok(self.claim_result.clone())
        }
        async fn apply_settlement(
            &self,
            _id: &ScheduleId,
            actual_fire_at: i64,
            action: &SettlementAction,
        ) -> Result<bool, PersistenceFailure> {
            self.calls.lock().unwrap().push(Call::Settle {
                action: action.clone(),
                actual_fire_at,
            });
            Ok(self.settlement_applies)
        }
    }

    impl ScheduleWake for Recorder {
        async fn deliver(&self, ctx: WakeContext<'_>) -> Result<(), WakeFailed> {
            let c = ctx.claimed;
            self.calls.lock().unwrap().push(Call::Deliver {
                actual_fire_at: ctx.actual_fire_at,
                owner_agent_id: c.delivery.owner_agent_id.as_str().to_owned(),
                target_conversation_id: c.delivery.target_conversation_id.as_str().to_owned(),
                title: c.delivery.title.as_str().to_owned(),
                message: c.delivery.message.as_str().to_owned(),
            });
            *self.wake_meta.lock().unwrap() = Some(wake_meta_of(&ctx));
            self.deliver_result.clone()
        }
    }

    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    /// Periodic cron identical to the parity `SteppingCron`.
    struct SteppingCron {
        step_ms: i64,
    }
    impl CronNextOccurrence for SteppingCron {
        fn next_after(&self, query: CronQuery<'_>) -> CronOutcome {
            CronOutcome::NextInstant((query.after / self.step_ms + 1) * self.step_ms)
        }
    }
    /// A cron that faults on every call — a `cron` expression that no longer
    /// resolves. Interval specs never consult it.
    struct BrokenCron;
    impl CronNextOccurrence for BrokenCron {
        fn next_after(&self, _query: CronQuery<'_>) -> CronOutcome {
            CronOutcome::InvalidExpression
        }
    }

    fn delivery() -> DeliveryMaterial {
        DeliveryMaterial {
            owner_agent_id: AgentId::parse(OWNER).unwrap(),
            target_conversation_id: ConversationId::parse(TARGET).unwrap(),
            title: ScheduleTitle::parse(TITLE).unwrap(),
            message: ScheduleMessage::parse(MESSAGE).unwrap(),
        }
    }

    /// The authoritative fact a winning claim returns. The candidate that drives
    /// `process_one` is deliberately separate: these are the coordinates of the
    /// occurrence actually WON, which is all `process_one` may read.
    fn claimed(
        spec: ScheduleSpec,
        caps: ScheduleCaps,
        fired: i64,
        scheduled_fire_at: i64,
    ) -> ClaimedSchedule {
        ClaimedSchedule {
            id: ScheduleId::parse("sched-1").unwrap(),
            spec,
            caps,
            fired_count: FiredCount::new(fired).unwrap(),
            scheduled_fire_at,
            delivery: delivery(),
        }
    }

    /// A fixed pre-claim candidate. Its coordinates are intentionally NOT the
    /// ones the claim returns in most tests — proving the candidate is only the
    /// claim predicate, never a policy input.
    fn candidate() -> DueCandidate {
        DueCandidate {
            id: ScheduleId::parse("sched-1").unwrap(),
            expected_fired_count: FiredCount::new(0).unwrap(),
            expected_next_fire_at: 1_000,
        }
    }

    fn deliver_call(actual_fire_at: i64) -> Call {
        Call::Deliver {
            actual_fire_at,
            owner_agent_id: OWNER.into(),
            target_conversation_id: TARGET.into(),
            title: TITLE.into(),
            message: MESSAGE.into(),
        }
    }

    fn once() -> ScheduleSpec {
        ScheduleSpec::Once { fire_at: 1_000 }
    }
    fn interval() -> ScheduleSpec {
        ScheduleSpec::Interval {
            every: IntervalSeconds::new(60).unwrap(),
            start_at: None,
        }
    }
    fn cron_spec() -> ScheduleSpec {
        ScheduleSpec::Cron(super::super::spec::CronSpec::new(
            "*/5 * * * *".into(),
            "UTC".into(),
        ))
    }
    fn no_caps() -> ScheduleCaps {
        ScheduleCaps::none()
    }

    async fn run(
        rec: &Recorder,
        cron: &impl CronNextOccurrence,
        now: i64,
    ) -> Result<ProcessOutcome, ProcessError> {
        process_one(rec, rec, &FixedClock(now), cron, &candidate()).await
    }

    #[tokio::test]
    async fn lost_claim_reads_no_clock_delivers_nothing_and_never_settles() {
        struct UnreachableClock;
        impl Clock for UnreachableClock {
            fn now(&self) -> i64 {
                panic!("a lost claim must not read the fire clock");
            }
        }
        let rec = Recorder::claim_lost();
        let out = process_one(
            &rec,
            &rec,
            &UnreachableClock,
            &SteppingCron { step_ms: 1 },
            &candidate(),
        )
        .await
        .unwrap();
        assert_eq!(out, ProcessOutcome::ClaimLost);
        // The ONLY call is the claim attempt: no deliver, no settle.
        assert_eq!(rec.calls(), vec![Call::Claim]);
    }

    #[tokio::test]
    async fn delivered_once_claims_then_delivers_then_marks_done_exactly_once() {
        let rec = Recorder::won(claimed(once(), no_caps(), 0, 1_000));
        let out = run(&rec, &SteppingCron { step_ms: 1 }, 5_000)
            .await
            .unwrap();
        assert_eq!(
            out,
            ProcessOutcome::Settled {
                action: SettlementAction::MarkDone,
                applied: true
            }
        );
        // Order is load-bearing: claim, then deliver, then a single settle —
        // delivery saw the clock's actual fire instant and the claim's material.
        assert_eq!(
            rec.calls(),
            vec![
                Call::Claim,
                deliver_call(5_000),
                Call::Settle {
                    action: SettlementAction::MarkDone,
                    actual_fire_at: 5_000
                },
            ]
        );
    }

    #[tokio::test]
    async fn delivered_interval_rearms_from_the_actual_fire_instant() {
        let rec = Recorder::won(claimed(interval(), no_caps(), 0, 100_000));
        // fires at 100_000; interval 60s → next 160_000.
        let out = run(&rec, &SteppingCron { step_ms: 1 }, 100_000)
            .await
            .unwrap();
        assert_eq!(
            out,
            ProcessOutcome::Settled {
                action: SettlementAction::Rearm {
                    next_fire_at: 160_000
                },
                applied: true
            }
        );
    }

    #[tokio::test]
    async fn failed_once_settles_mark_failed_with_the_reason() {
        let rec =
            Recorder::won(claimed(once(), no_caps(), 0, 1_000)).delivery_fails("transport down");
        let out = run(&rec, &SteppingCron { step_ms: 1 }, 5_000)
            .await
            .unwrap();
        assert_eq!(
            out,
            ProcessOutcome::Settled {
                action: SettlementAction::MarkFailed {
                    reason: "transport down".into()
                },
                applied: true
            }
        );
        // Still claimed → delivered(failed) → settled once. No compensation.
        assert_eq!(
            rec.calls(),
            vec![
                Call::Claim,
                deliver_call(5_000),
                Call::Settle {
                    action: SettlementAction::MarkFailed {
                        reason: "transport down".into()
                    },
                    actual_fire_at: 5_000,
                },
            ]
        );
    }

    #[tokio::test]
    async fn failed_repeating_rearms_fire_failed_and_stays_live() {
        let rec = Recorder::won(claimed(interval(), no_caps(), 0, 100_000)).delivery_fails("boom");
        let out = run(&rec, &SteppingCron { step_ms: 1 }, 100_000)
            .await
            .unwrap();
        assert_eq!(
            out,
            ProcessOutcome::Settled {
                action: SettlementAction::MarkFireFailed {
                    reason: "boom".into(),
                    next_fire_at: 160_000
                },
                applied: true
            }
        );
    }

    #[tokio::test]
    async fn failed_repeating_past_until_marks_plan_ended_after_failure() {
        // next tick 160_000 > until 120_000 → terminal, but recorded as failure.
        let caps = ScheduleCaps::new(None, Some(120_000)).unwrap();
        let rec = Recorder::won(claimed(interval(), caps, 0, 100_000)).delivery_fails("boom");
        let out = run(&rec, &SteppingCron { step_ms: 1 }, 100_000)
            .await
            .unwrap();
        assert_eq!(
            out,
            ProcessOutcome::Settled {
                action: SettlementAction::MarkPlanEndedAfterFailure {
                    reason: "boom".into()
                },
                applied: true
            }
        );
    }

    #[tokio::test]
    async fn cancel_race_makes_the_settlement_a_benign_no_op() {
        // The wake delivered; a cancel flipped the row to `cancelled` before our
        // settlement CAS, so the store reports it applied to nothing. The
        // orchestrator accepts that and does NOT retry or escalate.
        let rec = Recorder::won(claimed(interval(), no_caps(), 0, 100_000)).cancel_race();
        let out = run(&rec, &SteppingCron { step_ms: 1 }, 100_000)
            .await
            .unwrap();
        assert_eq!(
            out,
            ProcessOutcome::Settled {
                action: SettlementAction::Rearm {
                    next_fire_at: 160_000
                },
                applied: false
            }
        );
        // Exactly one settlement attempt despite the no-op; no compensation.
        assert_eq!(
            rec.calls(),
            vec![
                Call::Claim,
                deliver_call(100_000),
                Call::Settle {
                    action: SettlementAction::Rearm {
                        next_fire_at: 160_000
                    },
                    actual_fire_at: 100_000,
                },
            ]
        );
    }

    #[tokio::test]
    async fn process_one_plans_from_the_claimed_fact_not_the_candidate_snapshot() {
        // P0-1 (occurrence-ABA) at the port level. The candidate believes this
        // is occurrence 0 (see `candidate()`); the row the claim actually won is
        // occurrence 4 of a max-5 plan. Planning MUST use the CLAIMED
        // fired_count (4 → new 5 == max → terminal → delivered MarkDone), not the
        // stale candidate's (0 → new 1 < max → would wrongly Rearm). The
        // distinct outcome is the proof that `process_one` never reads the
        // candidate's coordinates after the claim.
        let caps = ScheduleCaps::new(Some(5), None).unwrap();
        let rec = Recorder::won(claimed(interval(), caps, 4, 999_000));
        let out = run(&rec, &SteppingCron { step_ms: 1 }, 1_000_000)
            .await
            .unwrap();
        assert_eq!(
            out,
            ProcessOutcome::Settled {
                action: SettlementAction::MarkDone,
                applied: true
            }
        );
    }

    #[tokio::test]
    async fn wake_receives_the_complete_claimed_occurrence_metadata() {
        // The wake fact is the WHOLE claimed occurrence, asserted field-for-field
        // against the TS `deliverWake` metadata set — NOT a 3-field subset that
        // could let the future adapter silently drop `spec`, the planned instant,
        // or the occurrence-label inputs. A repeating interval fired on time at
        // occurrence 2 of a max-5 plan: it rearms (next instant present) and the
        // occ label reads fired_count=2 / max=5. Bespoke routing/payload prove
        // every field rode the winning claim, not a re-read (P0-2: the Recorder
        // has no ScheduleId→payload capability at all).
        let claimed = ClaimedSchedule {
            id: ScheduleId::parse("sched-9").unwrap(),
            spec: interval(), // 60s
            caps: ScheduleCaps::new(Some(5), None).unwrap(),
            fired_count: FiredCount::new(2).unwrap(),
            scheduled_fire_at: 100_000,
            delivery: DeliveryMaterial {
                owner_agent_id: AgentId::parse("agent-42").unwrap(),
                target_conversation_id: ConversationId::parse("conv-77").unwrap(),
                title: ScheduleTitle::parse("weekly report").unwrap(),
                message: ScheduleMessage::parse("the numbers").unwrap(),
            },
        };
        let rec = Recorder::won(claimed);
        // Fires exactly on time (actual == scheduled == 100_000): missed 0;
        // interval rearms from the ACTUAL instant → 100_000 + 60s = 160_000.
        let _ = run(&rec, &SteppingCron { step_ms: 1 }, 100_000)
            .await
            .unwrap();
        assert_eq!(
            rec.wake_meta().unwrap(),
            WakeMeta {
                schedule_id: "sched-9".into(),
                spec_kind: "interval",
                spec_summary: "interval 60s".into(),
                planned_fire_at: 100_000,
                actual_fire_at: 100_000,
                missed_count: 0,
                next_fire_at: Some(160_000),
                fired_count: 2,
                max_occurrences: Some(5),
                owner_agent_id: "agent-42".into(),
                target_conversation_id: "conv-77".into(),
                title: "weekly report".into(),
                message: "the numbers".into(),
            }
        );
    }

    #[tokio::test]
    async fn wake_metadata_for_a_terminal_once_carries_no_next_instant() {
        // The terminal arm of the wake fact: a `once` fire has no future instant,
        // so `next_fire_at` is None and `max_occurrences` is absent, while the
        // planned/actual instants and the `once` summary still ride through. This
        // pins the Terminal → None mapping the repeating case cannot exercise.
        let rec = Recorder::won(claimed(once(), no_caps(), 0, 1_000));
        let _ = run(&rec, &SteppingCron { step_ms: 1 }, 5_000)
            .await
            .unwrap();
        assert_eq!(
            rec.wake_meta().unwrap(),
            WakeMeta {
                schedule_id: "sched-1".into(),
                spec_kind: "once",
                spec_summary: "once".into(),
                planned_fire_at: 1_000,
                actual_fire_at: 5_000,
                missed_count: 0,
                next_fire_at: None,
                fired_count: 0,
                max_occurrences: None,
                owner_agent_id: OWNER.into(),
                target_conversation_id: TARGET.into(),
                title: TITLE.into(),
                message: MESSAGE.into(),
            }
        );
    }

    #[test]
    fn delivery_text_decode_matches_the_wire_utf16_contract() {
        // The decode ceiling is the wire's EXACT unit — UTF-16 code units, the
        // same count Zod `.max()` applies (JS `String.length`) — not a wider
        // scalar-count budget. "😀" is one Unicode scalar but TWO UTF-16 units.
        let astral = "😀";
        assert_eq!(astral.encode_utf16().count(), 2);

        // title: 60 astral = 120 units → at ceiling, accepted; 61 = 122 → over.
        assert!(ScheduleTitle::parse(astral.repeat(60)).is_ok());
        assert_eq!(
            ScheduleTitle::parse(astral.repeat(61)),
            Err(InvalidDeliveryText::TitleTooLong)
        );
        // message: 250 astral = 500 units → at ceiling; 251 = 502 → over.
        assert!(ScheduleMessage::parse(astral.repeat(250)).is_ok());
        assert_eq!(
            ScheduleMessage::parse(astral.repeat(251)),
            Err(InvalidDeliveryText::MessageTooLong)
        );
        // ASCII exact ceiling is accepted (1 unit per char).
        assert!(ScheduleTitle::parse("t".repeat(SCHEDULE_TITLE_MAX_LEN)).is_ok());
        assert!(ScheduleMessage::parse("m".repeat(SCHEDULE_MESSAGE_MAX_LEN)).is_ok());

        // Empty is corruption on both.
        assert_eq!(
            ScheduleTitle::parse(""),
            Err(InvalidDeliveryText::EmptyTitle)
        );
        assert_eq!(
            ScheduleMessage::parse(""),
            Err(InvalidDeliveryText::EmptyMessage)
        );

        // The wire persists TRIMMED titles: surrounding whitespace and
        // all-whitespace are rows create could not have written → corruption,
        // never silently re-trimmed.
        assert_eq!(
            ScheduleTitle::parse(" title "),
            Err(InvalidDeliveryText::TitleNotTrimmed)
        );
        assert_eq!(
            ScheduleTitle::parse("   "),
            Err(InvalidDeliveryText::TitleNotTrimmed)
        );

        // Cross-language trim contract: the canonical check is the EXACT
        // ECMAScript trim set, not Rust's Unicode `White_Space`, which disagrees
        // at the boundary in both directions.
        //   - U+FEFF (ZWNBSP) IS ES WhiteSpace: JS `trim()` removes it, so
        //     `create` cannot persist FEFF padding → corruption on decode.
        //     (Rust `str::trim` would keep it and wrongly accept.)
        assert_eq!(
            ScheduleTitle::parse("\u{FEFF}title\u{FEFF}"),
            Err(InvalidDeliveryText::TitleNotTrimmed)
        );
        assert_eq!(
            ScheduleTitle::parse("title\u{FEFF}"),
            Err(InvalidDeliveryText::TitleNotTrimmed)
        );
        //   - U+0085 (NEL) is NOT in the ES trim set: JS `trim()` leaves it, so
        //     a TS-persisted title may legitimately carry it → accepted and
        //     preserved verbatim. (Rust `str::trim` would strip it and wrongly
        //     reject this valid row.)
        assert_eq!(
            ScheduleTitle::parse("\u{0085}title").unwrap().as_str(),
            "\u{0085}title"
        );

        // The message is NOT trimmed by the wire: surrounding whitespace is
        // legitimate content, preserved verbatim.
        assert_eq!(
            ScheduleMessage::parse("  padded  ").unwrap().as_str(),
            "  padded  "
        );
    }

    #[tokio::test]
    async fn pre_delivery_cadence_fault_abandons_the_fire_before_delivering() {
        // A claimed cron fire whose expression no longer resolves: the success
        // plan faults BEFORE delivery, so nothing is delivered and nothing is
        // settled — the row is left `firing` for boot recovery.
        let rec = Recorder::won(claimed(cron_spec(), no_caps(), 0, 0));
        let err = run(&rec, &BrokenCron, 10_000).await.unwrap_err();
        assert!(matches!(err, ProcessError::Advance(_)));
        // A source chain reaches the operator cause, not just a Debug variant.
        assert!(std::error::Error::source(&err).is_some());
        // Claimed, then abandoned: no deliver, no settle.
        assert_eq!(rec.calls(), vec![Call::Claim]);
    }

    #[tokio::test]
    async fn post_failure_cadence_fault_abandons_after_delivery_without_settling() {
        // Delivery FAILED and then the failed-fire rearm faults. This is only
        // reachable when the SUCCESS plan went terminal-by-max (so advancement
        // never computed a cadence instant — the max branch's only cron use is
        // the swallowed missed-count hint), yet the failure path ignores max and
        // DOES consult the now-broken cron. The row is left `firing` (recovered
        // on boot), never half-settled.
        // fired 0, max 1 → new_count 1 >= 1 → advancement terminal, no instant.
        let caps = ScheduleCaps::new(Some(1), None).unwrap();
        let rec = Recorder::won(claimed(cron_spec(), caps, 0, 0)).delivery_fails("boom");
        let err = run(&rec, &BrokenCron, 10_000).await.unwrap_err();
        assert!(matches!(err, ProcessError::FailureRearm(_)));
        assert!(std::error::Error::source(&err).is_some());
        // Claim + deliver happened; the settlement write did NOT.
        assert_eq!(rec.calls(), vec![Call::Claim, deliver_call(10_000)]);
    }
}
