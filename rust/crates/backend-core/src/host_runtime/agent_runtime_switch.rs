//! The agent runtime-switch saga: change one agent's runtime driver without changing its
//! identity, losing a live session, or racing a delete.
//!
//! Switching drivers is a lifecycle transition, not a column edit. The old-runtime sessions
//! must be cleared before the `runtime` column moves, and nothing may spawn or deliver under
//! the agent while that window is open. This module orchestrates that window against narrow
//! typed ports; the durable half — the compare-and-set lock lifecycle, the committed-fact
//! seal, and the lock-fenced seat/inbox reads — lives in [`crate::host_runtime::runtime_transition`] and is
//! reused verbatim here.
//!
//! # Shape of the transition (mirrors the TypeScript `switchAgentRuntime`)
//!
//!  1. **Preflight** (no lock): the row exists, the target differs from the current runtime,
//!     the target is selectable, and a forced host catalog probe proves it is not
//!     deterministically absent. Availability is host-local truth — only a definite `absent`
//!     blocks; `present_unverified`/`unknown` defer to the daemon spawn last-line.
//!  2. **Acquire** the durable transition lock ([`begin_runtime_transition`]). A concurrent
//!     switch or an in-flight delete loses here, loudly, instead of interleaving.
//!  3. **Under the lock**: re-read the row (the preflight snapshot can go stale across the
//!     catalog round-trip); enumerate every SEAT through the lock-fenced sealed read; capture
//!     the AgentSlot presentation focus while its custody is still current; reclaim in-flight
//!     inbox leases; and clear every old-runtime session. A single clear failure aborts the
//!     lock and leaves the runtime unchanged — no partial switch is ever committed.
//!  4. **Commit** the runtime + tuning in one guarded write ([`complete_runtime_transition`]),
//!     which also releases the lock. A `Lost` commit means another attempt owns the lock now.
//!  5. **Re-drive** (post-commit): start the conversations that must resume under the new
//!     runtime. Nothing here can revert the committed transition. The known-live and sealed
//!     reclaimed conversations are always dispatched; each supplemental resume-plan read
//!     (newly-pending, oldest-pending, participation) degrades INDEPENDENTLY — a fault skips
//!     only that read and is surfaced in `resume_read_faults`, never silently zeroed — and a
//!     restart the provider refuses is counted. The AgentSlot ready-replay / inbox trigger
//!     picks up whatever this best-effort pass could not enumerate.
//!
//! # Where the effect targets come from
//!
//! The per-seat clear here is a consequence of a transition the caller already authorized
//! (route edit-admission) and that the durable lock already fenced — not a fresh user action.
//! So the saga does NOT re-run the per-seat admission ladder: it mints each sealed
//! [`RuntimeSeatTarget`] only from a [`TransitionClearCandidate`] — the sole bridge from a
//! lock-fenced sealed seat to a clear target — so every cleared scope is provably one the
//! held marker enumerated, never a caller-picked arm.

use std::future::Future;

use futures_util::future::join_all;

use crate::{
    conversation_messaging::messaging::collaboration::{
        RuntimeControlPort, RuntimeSeatTarget, RuntimeStartTarget,
    },
    foundation::identity::{AgentId, ConversationId, HostId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::{
        AgentSlotPresentationFocusPort, RuntimeCustodyReadPort, RuntimeScope,
    },
    host_runtime::runtime_event::RuntimeObservedStatus,
    host_runtime::runtime_transition::{
        AbortTransition, BeginTransition, CommittedRuntimeTransition, CompleteTransition,
        ReadRuntimeTransitionSeats, ReclaimRuntimeTransitionInboxes, RuntimeTransitionCommit,
        RuntimeTransitionFault, RuntimeTransitionInboxWrite, RuntimeTransitionLock,
        RuntimeTransitionSeatRead, RuntimeTransitionWrite, TransitionClearCandidate,
        abort_runtime_transition, begin_runtime_transition, complete_runtime_transition,
        read_runtime_transition_seats, reclaim_runtime_transition_inboxes,
    },
    identity_access::agent_profile_write::{AgentChangedAnnounce, AgentChangedSink},
    identity_access::agents::{
        runtime::RuntimeKind, runtime_catalog::SELECTABLE_RUNTIMES,
        session_scope::AgentSessionScope,
    },
};

/// The validated runtime-switch request. Tuning legality (model/effort/service-tier against the
/// target runtime) is the upstream admission schema's job, mirrored from the TypeScript
/// `SwitchAgentRuntimeInputSchema`; this value carries the already-validated tuning verbatim
/// to the commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitchAgentRuntimeConfig {
    pub runtime: RuntimeKind,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
}

impl SwitchAgentRuntimeConfig {
    fn commit(&self) -> RuntimeTransitionCommit {
        RuntimeTransitionCommit {
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            service_tier: self.service_tier.clone(),
        }
    }
}

/// One agent + actor + the target runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitchAgentRuntimeRequest {
    pub agent: AgentId,
    pub actor: crate::foundation::identity::UserId,
    pub config: SwitchAgentRuntimeConfig,
}

/// The row facts the saga reads for the preflight and the authoritative under-lock re-read.
/// SEATS are NOT part of this snapshot: they are read separately through the lock-fenced
/// sealed [`RuntimeTransitionSeatRead`], so they can only be enumerated while the exact
/// transition marker is held.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitchAgentSnapshot {
    /// The id the row was read FROM — its own primary key. Bound against the requested agent so
    /// a snapshot resolved from the wrong row is a fault, never acted on.
    pub source: AgentId,
    /// The current (from) runtime, decoded from the row.
    pub runtime: RuntimeKind,
    pub session_scope: AgentSessionScope,
    pub host: HostId,
}

/// The single source-bound read the saga needs of the agent row. Used for both the preflight
/// read and the authoritative re-read under the lock.
pub trait SwitchAgentSnapshotRead: Clone + Send + Sync + 'static {
    fn read_switch_snapshot(
        &self,
        agent: &AgentId,
    ) -> impl Future<Output = Result<Option<SwitchAgentSnapshot>, PersistenceFailure>> + Send;
}

/// A forced host round-trip that answers only whether the target runtime is deterministically
/// absent on the agent's computer. The transport failure arms mirror the owning-host catalog
/// dispatch; the route maps each to its HTTP status.
pub trait RuntimeAvailabilityProbe: Clone + Send + Sync + 'static {
    fn probe_runtime_available(
        &self,
        host: &HostId,
        runtime: RuntimeKind,
    ) -> impl Future<Output = Result<RuntimeAvailabilityVerdict, RuntimeAvailabilityProbeFault>> + Send;
}

/// The only distinction the transition draws: a definite `Absent` blocks it; everything else
/// (`present` / `present_unverified` / `unknown`) defers to the daemon spawn last-line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeAvailabilityVerdict {
    Absent,
    AvailableOrUnknown,
}

/// A host catalog probe that could not produce a verdict. Each arm is the switch-domain view of
/// an owning-host data-plane failure; the route maps it to the same HTTP status the standalone
/// catalog route uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeAvailabilityProbeFault {
    /// No daemon currently owns the host (or a replacement attached mid-flight).
    AttachPending,
    /// Dispatched but unacknowledged within the deadline.
    Timeout,
    /// The daemon answered with an internal error.
    DaemonInternalError,
    /// The daemon's response violated the catalog transport contract.
    TransportContractViolation,
}

/// The conversation-inbox reads the saga needs for the POST-COMMIT re-drive: enumerate the
/// pending conversations to resume, and re-check a presentation focus against live
/// participation. Returning in-flight claims to pending is NOT here — that is the lock-fenced
/// [`RuntimeTransitionInboxWrite`], run under the marker before the commit.
///
/// `list_pending_for_agent` MUST return oldest-durable-pending first: the agent-scope restart
/// takes only its head.
pub trait SwitchConversationCoordination: Clone + Send + Sync + 'static {
    /// Every conversation with undelivered input for this agent, oldest durable pending first.
    fn list_pending_for_agent(
        &self,
        agent: &AgentId,
    ) -> impl Future<Output = Result<Vec<ConversationId>, PersistenceFailure>> + Send;

    /// Conversations that gained undelivered input at or after `since_ms` — the messages that
    /// arrived while this transition held the spawn gate.
    fn list_pending_for_agent_since(
        &self,
        agent: &AgentId,
        since_ms: i64,
    ) -> impl Future<Output = Result<Vec<ConversationId>, PersistenceFailure>> + Send;

    /// Whether the agent still participates in the conversation — the placement-aware authority,
    /// so a focus captured before clear is reused only while it remains a real binding.
    fn is_agent_participant(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;
}

/// A committed runtime switch and the effect tallies the wire reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitchAgentRuntimeCommitted {
    /// The sealed committed transition — the authority for the new runtime + tuning. The route
    /// reprojects the agent DTO from the committed row.
    pub committed: CommittedRuntimeTransition,
    pub cleared_sessions: usize,
    pub restarted_conversations: usize,
    pub restart_failures: usize,
    /// Post-commit resume-plan reads that faulted and were degraded past. NON-ZERO means the
    /// best-effort re-drive could not enumerate every conversation that might need restarting;
    /// the known-live/reclaimed candidates were still dispatched, but the supplemental reads
    /// (newly-pending, oldest-pending, participation) were skipped. The boundary must surface
    /// this as an operational signal — never report a silent `0` — so the aftermath fault is
    /// visible even though it cannot revert the committed switch.
    pub resume_read_faults: usize,
}

/// The OUTER orchestration outcome of a committed switch: the committed tallies paired with the
/// fate of the best-effort `agent_changed` announce fired AFTER the transition was already durable.
/// It mirrors the tuning PATCH's `AnnouncedAgentProfileUpdate`: the announce fate is `#[must_use]`
/// and reachable only through [`into_parts`](Self::into_parts), so the boundary taking the committed
/// tallies for its wire re-read receives the announce residue in the same move and cannot silently
/// drop it. A failed hint NEVER downgrades the committed switch — the boundary logs it and still
/// returns success — but it must be OBSERVED, never swallowed like the old `let _ =`.
#[derive(Debug)]
#[must_use = "a committed runtime switch carries an agent_changed announce fate that must be handled"]
pub struct AnnouncedRuntimeSwitch {
    committed: SwitchAgentRuntimeCommitted,
    announce: AgentChangedAnnounce,
}

impl AnnouncedRuntimeSwitch {
    /// Split into the committed tallies and the announce fate. The only way to read either, so a
    /// boundary reprojecting from the committed result also receives the announce it must log.
    pub fn into_parts(self) -> (SwitchAgentRuntimeCommitted, AgentChangedAnnounce) {
        (self.committed, self.announce)
    }
}

/// The product outcomes of a switch that did not commit. Ordering across simultaneously-true
/// conditions is deliberately NOT a migration contract (validator governance owns it); each arm
/// carries only its stable final semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwitchAgentRuntimeError {
    /// No such agent row (preflight or under-lock re-read).
    AgentNotFound,
    /// The agent already uses the requested runtime.
    RuntimeUnchanged,
    /// The requested runtime is not a user-selectable driver.
    RuntimeNotSelectable,
    /// The requested runtime is deterministically absent on the agent's computer.
    RuntimeNotInstalled,
    /// A transition lock is already held for this agent.
    TransitionInProgress,
    /// The agent is being deleted; a transition must not race teardown.
    DeletionInProgress,
    /// The guarded commit (or a lock-fenced read) matched no row — another attempt owns the
    /// lock now.
    TransitionLost,
    /// At least one old-runtime session could not be cleared; the runtime was not changed.
    SessionClearFailed,
    /// The host catalog probe could not produce a verdict.
    Availability(RuntimeAvailabilityProbeFault),
    /// An internal invariant break — a persistence error or a lock-lifecycle witness fault. The
    /// boundary surfaces every arm as a safe `500`, never a product answer.
    Fault(SwitchAgentRuntimeFault),
}

/// The internal-fault payload behind [`SwitchAgentRuntimeError::Fault`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwitchAgentRuntimeFault {
    /// A persistence read/write failed.
    Persistence,
    /// The snapshot resolved a row whose id is not the requested agent.
    SourceUntrustworthy,
    /// The custody port answered a `current` read for the requested agent's AgentSlot with a
    /// principal bound to a different scope/agent — a raw fact core must not consume as this
    /// agent's focus source.
    CustodyScopeMismatch,
    /// A lock-lifecycle or lock-fenced read returned a witness that disagreed with the request.
    Transition,
}

impl From<PersistenceFailure> for SwitchAgentRuntimeError {
    fn from(_: PersistenceFailure) -> Self {
        Self::Fault(SwitchAgentRuntimeFault::Persistence)
    }
}

impl From<RuntimeTransitionFault> for SwitchAgentRuntimeError {
    fn from(fault: RuntimeTransitionFault) -> Self {
        match fault {
            RuntimeTransitionFault::Persistence(_) => {
                Self::Fault(SwitchAgentRuntimeFault::Persistence)
            }
            RuntimeTransitionFault::LockWitnessMismatch { .. }
            | RuntimeTransitionFault::CommitWitnessMismatch { .. }
            | RuntimeTransitionFault::SeatSnapshotWitnessMismatch { .. }
            | RuntimeTransitionFault::InboxReclaimWitnessMismatch { .. } => {
                Self::Fault(SwitchAgentRuntimeFault::Transition)
            }
        }
    }
}

/// Transition one agent to another runtime. See the module docs for the phase shape and the
/// invariants each phase holds.
#[allow(clippy::too_many_arguments)]
pub async fn switch_agent_runtime<R, Avail, Coord, W, Seats, Inboxes, Cust, Focus, P, Changed>(
    reads: &R,
    availability: &Avail,
    coordination: &Coord,
    transitions: &W,
    seats: &Seats,
    inboxes: &Inboxes,
    custody: &Cust,
    focus: &Focus,
    control: &P,
    changed: &Changed,
    request: &SwitchAgentRuntimeRequest,
    now_ms: i64,
) -> Result<AnnouncedRuntimeSwitch, SwitchAgentRuntimeError>
where
    R: SwitchAgentSnapshotRead,
    Avail: RuntimeAvailabilityProbe,
    Coord: SwitchConversationCoordination,
    W: RuntimeTransitionWrite,
    Seats: RuntimeTransitionSeatRead,
    Inboxes: RuntimeTransitionInboxWrite,
    Cust: RuntimeCustodyReadPort,
    Focus: AgentSlotPresentationFocusPort,
    P: RuntimeControlPort,
    Changed: AgentChangedSink,
{
    let agent = &request.agent;
    let target = request.config.runtime;

    // ── Preflight (no lock) ──────────────────────────────────────────────
    let preflight = load_snapshot(reads, agent).await?;
    reject_unchanged_or_unselectable(&preflight, target)?;

    match availability
        .probe_runtime_available(&preflight.host, target)
        .await
    {
        Ok(RuntimeAvailabilityVerdict::AvailableOrUnknown) => {}
        Ok(RuntimeAvailabilityVerdict::Absent) => {
            return Err(SwitchAgentRuntimeError::RuntimeNotInstalled);
        }
        Err(fault) => return Err(SwitchAgentRuntimeError::Availability(fault)),
    }

    // ── Acquire the durable lock ─────────────────────────────────────────
    let lock = match begin_runtime_transition(transitions, agent, target, now_ms).await? {
        BeginTransition::Started(lock) => lock,
        BeginTransition::NotFound => return Err(SwitchAgentRuntimeError::AgentNotFound),
        BeginTransition::InProgress => return Err(SwitchAgentRuntimeError::TransitionInProgress),
        BeginTransition::Deleting => return Err(SwitchAgentRuntimeError::DeletionInProgress),
    };

    // ── Under the lock: everything below either commits or unwinds via `abort_and_restore`. ──
    match run_locked(
        reads,
        coordination,
        seats,
        inboxes,
        custody,
        focus,
        transitions,
        control,
        changed,
        request,
        now_ms,
        &lock,
    )
    .await
    {
        Ok(committed) => Ok(committed),
        Err(Unwound { error, restore }) => {
            // A committed transition already cleared the lock, so this abort is a target-guarded
            // no-op there; it only bites the pre-commit failure paths.
            if let Ok(AbortTransition::Released) =
                abort_runtime_transition(transitions, &lock).await
            {
                // Best effort: restore the previously-live pairs under the UNCHANGED old runtime.
                // Fan them out concurrently — as with the clear/restart passes, a serial loop would
                // amplify the abort aftermath to O(pair_count x per-start latency); the original
                // failure remains the response and a later retry can finish the switch.
                let restarts = restore
                    .into_iter()
                    .map(|conversation| {
                        RuntimeStartTarget::new(request.actor.clone(), agent.clone(), conversation)
                    })
                    .collect::<Vec<_>>();
                let _ = join_all(restarts.iter().map(|restart| control.start(restart))).await;
            }
            Err(error)
        }
    }
}

/// The error carried out of the under-lock body: the product/fault answer plus the pairs to
/// restore under the old runtime if the abort still holds the lock.
struct Unwound {
    error: SwitchAgentRuntimeError,
    restore: Vec<ConversationId>,
}

impl From<SwitchAgentRuntimeError> for Unwound {
    /// A failure discovered before any seat was cleared has nothing to restore.
    fn from(error: SwitchAgentRuntimeError) -> Self {
        Self {
            error,
            restore: Vec::new(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_locked<R, Coord, Seats, Inboxes, Cust, Focus, W, P, Changed>(
    reads: &R,
    coordination: &Coord,
    seats: &Seats,
    inboxes: &Inboxes,
    custody: &Cust,
    focus: &Focus,
    transitions: &W,
    control: &P,
    changed: &Changed,
    request: &SwitchAgentRuntimeRequest,
    now_ms: i64,
    lock: &RuntimeTransitionLock,
) -> Result<AnnouncedRuntimeSwitch, Unwound>
where
    R: SwitchAgentSnapshotRead,
    Coord: SwitchConversationCoordination,
    Seats: RuntimeTransitionSeatRead,
    Inboxes: RuntimeTransitionInboxWrite,
    Cust: RuntimeCustodyReadPort,
    Focus: AgentSlotPresentationFocusPort,
    W: RuntimeTransitionWrite,
    P: RuntimeControlPort,
    Changed: AgentChangedSink,
{
    let agent = &request.agent;
    let target = request.config.runtime;

    // Re-read under the lock: the preflight row can go stale across the catalog round-trip, so a
    // transition another request just committed to the same target must fail loudly here rather
    // than overwrite its tuning.
    let locked = load_snapshot(reads, agent).await?;
    reject_unchanged_or_unselectable(&locked, target).map_err(Unwound::from)?;

    // Enumerate the SEATS through the lock-fenced sealed read. The seal proves every seat
    // belongs to THIS held marker (a foreign-seat or wrong-lock snapshot is a fault, cleared
    // nothing); a `Lost` means the marker vanished under us — treat it as a lost transition.
    let snapshot = match read_runtime_transition_seats(seats, lock)
        .await
        .map_err(SwitchAgentRuntimeError::from)
        .map_err(Unwound::from)?
    {
        ReadRuntimeTransitionSeats::Snapshot(snapshot) => snapshot,
        ReadRuntimeTransitionSeats::Lost => {
            return Err(Unwound::from(SwitchAgentRuntimeError::TransitionLost));
        }
    };

    // Capture the AgentSlot presentation focus BEFORE any clear, while its custody is still
    // current. `current` then `focus` are two reads, not one atomic snapshot: if the slot exits
    // or finalizes between them the focus port re-checks custody and answers `None`, so the
    // worst case degrades to "no focus", never a successor's focus. A persistence fault on
    // either read fails closed here — before any clear or commit — with nothing to restore.
    let agent_focus = capture_agent_focus(custody, focus, agent, locked.session_scope)
        .await
        .map_err(Unwound::from)?;

    // Return every in-flight inbox lease to pending under the exact marker, so the post-commit
    // restart can redeliver under the target runtime. A `Lost` marker means we no longer own the
    // lock — nothing was written and nothing cleared.
    let reclaimed = match reclaim_runtime_transition_inboxes(inboxes, lock)
        .await
        .map_err(SwitchAgentRuntimeError::from)
        .map_err(Unwound::from)?
    {
        ReclaimRuntimeTransitionInboxes::Reclaimed(reclaim) => reclaim.conversations().to_vec(),
        ReclaimRuntimeTransitionInboxes::Lost => {
            return Err(Unwound::from(SwitchAgentRuntimeError::TransitionLost));
        }
    };

    // SEATS, not conversations: what must be cleared is every live-or-resumable SESSION, wherever
    // it sits. One row per seat, every placement — a DM or inbox session cannot be silently
    // excluded and switched out from under. The clear target is minted only from the sealed
    // candidate, so it is provably a scope this marker enumerated.
    let mut to_clear = Vec::new();
    let mut live_before: Vec<ConversationId> = Vec::new();
    let mut agent_slot_was_live = false;
    for candidate in snapshot.clear_candidates() {
        if !holds_session(&candidate) {
            continue;
        }
        match candidate.scope() {
            RuntimeScope::AgentSlot { .. } => {
                agent_slot_was_live = is_live(&candidate);
            }
            RuntimeScope::ConversationSlot { source, .. } => {
                if is_live(&candidate) {
                    live_before.push(source.clone());
                }
            }
        }
        to_clear.push(RuntimeSeatTarget::for_transition_clear(
            request.actor.clone(),
            &candidate,
        ));
    }

    // Clear every old-runtime session CONCURRENTLY, mirroring the TypeScript `Promise.allSettled`:
    // ALL are attempted (no short-circuit) so a stuck seat neither hides behind an earlier failure
    // nor serializes the clears into an O(seat_count x per-clear latency) lock-hold window. Every
    // outcome is then settled; a single failure (refusal or transport) aborts without committing.
    let clear_failed = join_all(
        to_clear
            .iter()
            .map(|seat_target| control.clear_session(seat_target)),
    )
    .await
    .into_iter()
    .any(|outcome| !matches!(outcome, Ok(Ok(_))));
    if clear_failed {
        return Err(Unwound {
            error: SwitchAgentRuntimeError::SessionClearFailed,
            restore: restore_targets(
                locked.session_scope,
                agent_slot_was_live,
                &agent_focus,
                &live_before,
            ),
        });
    }

    // ── Commit the runtime + tuning and release the lock in one guarded write. ──
    let committed =
        match complete_runtime_transition(transitions, lock, &request.config.commit(), now_ms)
            .await
            .map_err(SwitchAgentRuntimeError::from)
            .map_err(Unwound::from)?
        {
            CompleteTransition::Committed(committed) => committed,
            CompleteTransition::Lost => {
                return Err(Unwound {
                    error: SwitchAgentRuntimeError::TransitionLost,
                    // The lock is no longer ours; the target-guarded abort will find nothing and no
                    // restore fires. Cleared sessions stay cleared for whoever owns the lock now.
                    restore: Vec::new(),
                });
            }
        };

    // ── Announce the committed change BEFORE the possibly-blocking restart aftermath. ──
    // The runtime column has moved and the lock is released, so publish the app-scope
    // `agent_changed` hint exactly once, keyed on the SEALED committed agent id — the same
    // best-effort effect the tuning PATCH fires after its guarded commit. A delivery failure NEVER
    // flips the committed result, but it is CARRIED (never `let _ =`-dropped): the fate rides the
    // `#[must_use]` outer outcome to the boundary, which logs a `Failed` hint exactly as PATCH
    // does. Firing it before `dispatch_resume` keeps a slow restart pass from delaying the
    // notification.
    let announce = match changed.agent_changed(committed.agent_id()).await {
        Ok(()) => AgentChangedAnnounce::Delivered,
        Err(failure) => AgentChangedAnnounce::Failed(failure),
    };

    // ── Re-drive (post-commit): NEVER reverts the committed transition. ──
    // The plan foundation — live-before pairs and sealed reclaimed conversations — is already
    // KNOWN and is always dispatched. Each SUPPLEMENTAL read (newly-pending, oldest-pending,
    // participation) degrades INDEPENDENTLY: a fault skips only that read's contribution and is
    // counted in `resume_read_faults`, never silently zeroed and never dragging the known
    // candidates down with it. A provider restart refusal is likewise counted. The committed
    // switch stands regardless.
    let (restarted, restart_failures, resume_read_faults) = dispatch_resume(
        coordination,
        control,
        request,
        locked.session_scope,
        agent_slot_was_live,
        &reclaimed,
        &live_before,
        agent_focus,
        now_ms,
    )
    .await;

    Ok(AnnouncedRuntimeSwitch {
        committed: SwitchAgentRuntimeCommitted {
            committed,
            cleared_sessions: to_clear.len(),
            restarted_conversations: restarted,
            restart_failures,
            resume_read_faults,
        },
        announce,
    })
}

/// A seat still owns a live runtime child.
fn is_live(candidate: &TransitionClearCandidate<'_>) -> bool {
    matches!(
        candidate.observed_status(),
        RuntimeObservedStatus::Starting | RuntimeObservedStatus::Active
    )
}

/// Nothing running and nothing resumable — the old runtime holds no session behind this seat,
/// so the transition has nothing to clear through it.
fn holds_session(candidate: &TransitionClearCandidate<'_>) -> bool {
    is_live(candidate) || candidate.cli_session_id().is_some()
}

/// Capture the AgentSlot presentation focus while its custody is current, or `None`. Only
/// agent-scope agents have an AgentSlot focus; conversation-scope agents never do.
///
/// `RuntimeCustodyPort::current` returns a RAW fact: it is asked for THIS agent's AgentSlot, but
/// a lying (or racing) adapter could answer with a principal bound to another agent/scope. Core
/// binds the returned principal to the requested `AgentSlot{agent}` before handing it to the
/// focus port — otherwise another agent's focus could become this agent's fallback. A mismatch
/// is an internal witness fault that fails closed before any clear or commit.
async fn capture_agent_focus<Cust, Focus>(
    custody: &Cust,
    focus: &Focus,
    agent: &AgentId,
    session_scope: AgentSessionScope,
) -> Result<Option<ConversationId>, SwitchAgentRuntimeError>
where
    Cust: RuntimeCustodyReadPort,
    Focus: AgentSlotPresentationFocusPort,
{
    if session_scope != AgentSessionScope::Agent {
        return Ok(None);
    }
    let principal = match custody
        .current(RuntimeScope::AgentSlot {
            agent: agent.clone(),
        })
        .await?
    {
        Some(principal) => principal,
        None => return Ok(None),
    };
    // Source-bind the raw custody fact: it must be exactly this agent's AgentSlot principal.
    match principal.scope() {
        RuntimeScope::AgentSlot { agent: witnessed } if &witnessed == agent => {}
        _ => {
            return Err(SwitchAgentRuntimeError::Fault(
                SwitchAgentRuntimeFault::CustodyScopeMismatch,
            ));
        }
    }
    Ok(focus.agent_slot_presentation_focus(principal).await?)
}

/// The pairs to restart under the OLD runtime when a pre-commit failure aborts the lock.
fn restore_targets(
    session_scope: AgentSessionScope,
    agent_slot_was_live: bool,
    agent_focus: &Option<ConversationId>,
    live_before: &[ConversationId],
) -> Vec<ConversationId> {
    match session_scope {
        AgentSessionScope::Agent => match (agent_slot_was_live, agent_focus) {
            (true, Some(focus)) => vec![focus.clone()],
            _ => Vec::new(),
        },
        AgentSessionScope::Conversation => live_before.to_vec(),
    }
}

/// The post-commit re-drive. Infallible by contract: it returns `(restarted, restart_failures,
/// resume_read_faults)`. The known-live/reclaimed candidates are always dispatched; supplemental
/// resume-plan reads degrade independently and their faults are counted, never zeroed. Nothing
/// here reverts the committed switch.
#[allow(clippy::too_many_arguments)]
async fn dispatch_resume<Coord, P>(
    coordination: &Coord,
    control: &P,
    request: &SwitchAgentRuntimeRequest,
    session_scope: AgentSessionScope,
    agent_slot_was_live: bool,
    reclaimed: &[ConversationId],
    live_before: &[ConversationId],
    agent_focus: Option<ConversationId>,
    now_ms: i64,
) -> (usize, usize, usize)
where
    Coord: SwitchConversationCoordination,
    P: RuntimeControlPort,
{
    let (resume, resume_read_faults) = resume_targets(
        coordination,
        &request.agent,
        session_scope,
        agent_slot_was_live,
        reclaimed,
        live_before,
        agent_focus,
        now_ms,
    )
    .await;

    // Restart the resume set CONCURRENTLY (count-only, no rollback): the committed switch stands
    // no matter how many restarts succeed, so — like the clear pass — a serial loop would only
    // amplify the request latency to O(conversation_count x per-start latency). Each provider
    // refusal or transport error is counted, never retried and never reverting the switch.
    let restarts = resume
        .into_iter()
        .map(|conversation| {
            RuntimeStartTarget::new(request.actor.clone(), request.agent.clone(), conversation)
        })
        .collect::<Vec<_>>();
    let mut restarted = 0usize;
    let mut restart_failures = 0usize;
    for outcome in join_all(restarts.iter().map(|restart| control.start(restart))).await {
        match outcome {
            Ok(Ok(_)) => restarted += 1,
            Ok(Err(_)) | Err(_) => restart_failures += 1,
        }
    }
    (restarted, restart_failures, resume_read_faults)
}

/// The conversations to re-drive after a committed switch, plus the count of supplemental reads
/// that faulted. INFALLIBLE: the known-live/reclaimed foundation is always returned; each
/// supplemental read degrades on its own and increments the fault count instead of dropping the
/// whole plan.
#[allow(clippy::too_many_arguments)]
async fn resume_targets<Coord: SwitchConversationCoordination>(
    coordination: &Coord,
    agent: &AgentId,
    session_scope: AgentSessionScope,
    agent_slot_was_live: bool,
    reclaimed: &[ConversationId],
    live_before: &[ConversationId],
    agent_focus: Option<ConversationId>,
    now_ms: i64,
) -> (Vec<ConversationId>, usize) {
    match session_scope {
        AgentSessionScope::Agent => {
            // One AgentSlot gets one restart target. Origin is presentation only: prefer the
            // oldest durable pending origin (covers both pre-existing backlog and gate-window
            // arrivals), otherwise reuse the focus captured before clear — but only while it is
            // still a real participant. Starting once is enough; the AgentSlot ready-replay
            // drains the rest in inbox order. Each read degrades on its own.
            let mut faults = 0usize;
            match coordination.list_pending_for_agent(agent).await {
                Ok(pending) => {
                    if let Some(oldest) = pending.into_iter().next() {
                        return (vec![oldest], faults);
                    }
                }
                Err(_) => faults += 1,
            }
            // No pending origin (empty, or that read faulted). Fall back to the pre-clear focus,
            // but only when a known fact says this slot should resume at all.
            let should_fallback = agent_slot_was_live || !reclaimed.is_empty();
            if let Some(focus) = agent_focus.filter(|_| should_fallback) {
                match coordination.is_agent_participant(&focus, agent).await {
                    Ok(true) => return (vec![focus], faults),
                    Ok(false) => {}
                    Err(_) => faults += 1,
                }
            }
            (Vec::new(), faults)
        }
        AgentSessionScope::Conversation => {
            // KNOWN foundation first: the live pairs and the sealed reclaimed claims are already
            // determined and are always dispatched. Then nudge only messages that arrived while
            // this transition held the spawn gate — never turn historical backlog into a restart
            // herd. Deduped, insertion order preserved; a fault on the supplemental newly-pending
            // read is counted, leaving the known foundation intact.
            let mut resume: Vec<ConversationId> = Vec::new();
            let mut faults = 0usize;
            for conversation in live_before.iter().chain(reclaimed.iter()) {
                if !resume.contains(conversation) {
                    resume.push(conversation.clone());
                }
            }
            match coordination
                .list_pending_for_agent_since(agent, now_ms)
                .await
            {
                Ok(newly_pending) => {
                    for conversation in &newly_pending {
                        if !resume.contains(conversation) {
                            resume.push(conversation.clone());
                        }
                    }
                }
                Err(_) => faults += 1,
            }
            (resume, faults)
        }
    }
}

/// Read a snapshot and bind its source to the requested agent. A snapshot whose row id is not the
/// requested agent is a fault, not a product answer.
async fn load_snapshot<R: SwitchAgentSnapshotRead>(
    reads: &R,
    agent: &AgentId,
) -> Result<SwitchAgentSnapshot, SwitchAgentRuntimeError> {
    let snapshot = reads
        .read_switch_snapshot(agent)
        .await?
        .ok_or(SwitchAgentRuntimeError::AgentNotFound)?;
    if &snapshot.source != agent {
        return Err(SwitchAgentRuntimeError::Fault(
            SwitchAgentRuntimeFault::SourceUntrustworthy,
        ));
    }
    Ok(snapshot)
}

/// The two row-level product refusals shared by the preflight and the under-lock re-read.
fn reject_unchanged_or_unselectable(
    snapshot: &SwitchAgentSnapshot,
    target: RuntimeKind,
) -> Result<(), SwitchAgentRuntimeError> {
    if snapshot.runtime == target {
        return Err(SwitchAgentRuntimeError::RuntimeUnchanged);
    }
    if !SELECTABLE_RUNTIMES.contains(&target) {
        return Err(SwitchAgentRuntimeError::RuntimeNotSelectable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use crate::{
        conversation_messaging::messaging::collaboration::{
            ClearSessionOutcome, ClearSessionProviderRefusal, RuntimePlacementRefusal,
            RuntimeSeatTarget, StartRuntimeOutcome, StartRuntimeProviderRefusal,
        },
        foundation::identity::{DaemonInstanceId, UserId},
        host_runtime::runtime_credential::{RuntimeCustody, RuntimeGeneration, RuntimePrincipal},
        host_runtime::runtime_transition::{
            BeginTransitionRecord, CompleteTransitionRecord, RuntimeTransitionAfterImage,
            RuntimeTransitionInboxReclaimRecord, RuntimeTransitionInboxReclaimRow,
            RuntimeTransitionLockRow, RuntimeTransitionSeatRow,
            RuntimeTransitionSeatSnapshotRecord, RuntimeTransitionSeatSnapshotRow,
        },
    };

    use super::*;

    // ── identity helpers ────────────────────────────────────────────────

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }
    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }
    fn host(id: &str) -> HostId {
        HostId::parse(id).expect("host id")
    }
    fn conv(id: &str) -> ConversationId {
        ConversationId::parse(id).expect("conversation id")
    }

    const NOW: i64 = 1_000;
    const AGENT: &str = "agent-1";
    const ACTOR: &str = "user-1";
    const HOST: &str = "host-1";

    /// A stand-in persistence fault for the read-error fakes.
    fn boom() -> PersistenceFailure {
        PersistenceFailure::from_source(NeverFails)
    }

    fn request(from: RuntimeKind, to: RuntimeKind) -> (SwitchAgentRuntimeRequest, RuntimeKind) {
        (
            SwitchAgentRuntimeRequest {
                agent: agent(AGENT),
                actor: user(ACTOR),
                config: SwitchAgentRuntimeConfig {
                    runtime: to,
                    model: Some("gpt".into()),
                    reasoning_effort: None,
                    service_tier: None,
                },
            },
            from,
        )
    }

    fn snapshot(from: RuntimeKind, scope: AgentSessionScope) -> SwitchAgentSnapshot {
        SwitchAgentSnapshot {
            source: agent(AGENT),
            runtime: from,
            session_scope: scope,
            host: host(HOST),
        }
    }

    // ── sealed-seat witnesses (built for the lock the committing writer mints) ──

    /// The exact lock row `Transitions::committing` acquires: agent AGENT, target codex, at NOW.
    fn lock_row() -> RuntimeTransitionLockRow {
        RuntimeTransitionLockRow {
            agent_id: agent(AGENT),
            target: "codex".into(),
            started_at: NOW,
        }
    }

    fn conv_seat(
        id: &str,
        status: RuntimeObservedStatus,
        resumable: bool,
    ) -> RuntimeTransitionSeatRow {
        RuntimeTransitionSeatRow {
            scope: RuntimeScope::ConversationSlot {
                agent: agent(AGENT),
                source: conv(id),
            },
            observed_status: status,
            cli_session_id: resumable.then(|| id.to_string()),
        }
    }

    fn agent_seat(status: RuntimeObservedStatus, resumable: bool) -> RuntimeTransitionSeatRow {
        RuntimeTransitionSeatRow {
            scope: RuntimeScope::AgentSlot {
                agent: agent(AGENT),
            },
            observed_status: status,
            cli_session_id: resumable.then(|| "agent-seat".to_string()),
        }
    }

    // ── snapshot read fake: a queue consumed preflight-then-locked ───────

    type ScriptedSnapshot = Result<Option<SwitchAgentSnapshot>, PersistenceFailure>;

    #[derive(Clone)]
    struct SnapshotReads {
        queued: Arc<Mutex<VecDeque<ScriptedSnapshot>>>,
        calls: Arc<Mutex<usize>>,
    }

    impl SnapshotReads {
        /// Same snapshot answered for both the preflight and the under-lock re-read.
        fn stable(snapshot: SwitchAgentSnapshot) -> Self {
            Self::scripted(vec![Ok(Some(snapshot.clone())), Ok(Some(snapshot))])
        }
        fn scripted(answers: Vec<ScriptedSnapshot>) -> Self {
            Self {
                queued: Arc::new(Mutex::new(answers.into())),
                calls: Arc::new(Mutex::new(0)),
            }
        }
        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl SwitchAgentSnapshotRead for SnapshotReads {
        async fn read_switch_snapshot(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<SwitchAgentSnapshot>, PersistenceFailure> {
            *self.calls.lock().unwrap() += 1;
            self.queued.lock().unwrap().pop_front().unwrap_or(Ok(None))
        }
    }

    // ── availability probe fake ──────────────────────────────────────────

    #[derive(Clone)]
    struct Probe {
        verdict: Result<RuntimeAvailabilityVerdict, RuntimeAvailabilityProbeFault>,
        calls: Arc<Mutex<usize>>,
    }
    impl Probe {
        fn available() -> Self {
            Self::new(Ok(RuntimeAvailabilityVerdict::AvailableOrUnknown))
        }
        fn new(verdict: Result<RuntimeAvailabilityVerdict, RuntimeAvailabilityProbeFault>) -> Self {
            Self {
                verdict,
                calls: Arc::new(Mutex::new(0)),
            }
        }
        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }
    impl RuntimeAvailabilityProbe for Probe {
        async fn probe_runtime_available(
            &self,
            _host: &HostId,
            _runtime: RuntimeKind,
        ) -> Result<RuntimeAvailabilityVerdict, RuntimeAvailabilityProbeFault> {
            *self.calls.lock().unwrap() += 1;
            self.verdict
        }
    }

    // ── conversation coordination fake ───────────────────────────────────

    #[derive(Clone, Default)]
    struct Coordination {
        pending: Vec<ConversationId>,
        pending_since: Vec<ConversationId>,
        participant: bool,
        /// When set, `list_pending_for_agent_since` faults — a post-commit read failure.
        fault_pending_since: bool,
        /// When set, `list_pending_for_agent` faults — the AgentSlot oldest-origin read failure.
        fault_pending: bool,
    }
    impl SwitchConversationCoordination for Coordination {
        async fn list_pending_for_agent(
            &self,
            _agent: &AgentId,
        ) -> Result<Vec<ConversationId>, PersistenceFailure> {
            if self.fault_pending {
                return Err(boom());
            }
            Ok(self.pending.clone())
        }
        async fn list_pending_for_agent_since(
            &self,
            _agent: &AgentId,
            _since_ms: i64,
        ) -> Result<Vec<ConversationId>, PersistenceFailure> {
            if self.fault_pending_since {
                return Err(boom());
            }
            Ok(self.pending_since.clone())
        }
        async fn is_agent_participant(
            &self,
            _conversation: &ConversationId,
            _agent: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.participant)
        }
    }

    // ── lock-fenced seat read + inbox reclaim fakes ──────────────────────

    #[derive(Clone)]
    struct TransitionFacts {
        seats: RuntimeTransitionSeatSnapshotRecord,
        reclaim: RuntimeTransitionInboxReclaimRecord,
        seat_reads: Arc<Mutex<usize>>,
        reclaims: Arc<Mutex<usize>>,
    }
    impl TransitionFacts {
        fn new(seats: Vec<RuntimeTransitionSeatRow>, reclaimed: Vec<ConversationId>) -> Self {
            Self {
                seats: RuntimeTransitionSeatSnapshotRecord::Snapshot(
                    RuntimeTransitionSeatSnapshotRow {
                        lock: lock_row(),
                        seats,
                    },
                ),
                reclaim: RuntimeTransitionInboxReclaimRecord::Reclaimed(
                    RuntimeTransitionInboxReclaimRow {
                        lock: lock_row(),
                        conversations: reclaimed,
                    },
                ),
                seat_reads: Arc::new(Mutex::new(0)),
                reclaims: Arc::new(Mutex::new(0)),
            }
        }
        /// No seats, nothing reclaimed — for preflight/lock refusals that never read seats.
        fn empty() -> Self {
            Self::new(Vec::new(), Vec::new())
        }
        fn with_seats(seats: Vec<RuntimeTransitionSeatRow>) -> Self {
            Self::new(seats, Vec::new())
        }
        fn reads(&self) -> usize {
            *self.seat_reads.lock().unwrap()
        }
    }
    impl RuntimeTransitionSeatRead for TransitionFacts {
        async fn read_transition_seats(
            &self,
            _lock: &RuntimeTransitionLock,
        ) -> Result<RuntimeTransitionSeatSnapshotRecord, PersistenceFailure> {
            *self.seat_reads.lock().unwrap() += 1;
            Ok(self.seats.clone())
        }
    }
    impl RuntimeTransitionInboxWrite for TransitionFacts {
        async fn reclaim_transition_inboxes(
            &self,
            _lock: &RuntimeTransitionLock,
        ) -> Result<RuntimeTransitionInboxReclaimRecord, PersistenceFailure> {
            *self.reclaims.lock().unwrap() += 1;
            Ok(self.reclaim.clone())
        }
    }

    // ── custody + focus fakes ────────────────────────────────────────────

    fn agent_principal() -> RuntimePrincipal {
        RuntimePrincipal::AgentSlot {
            agent: agent(AGENT),
            custody: RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        }
    }

    /// A live AgentSlot custody for a DIFFERENT agent — a raw `current` fact core must reject as
    /// AGENT's focus source.
    fn other_agent_principal() -> RuntimePrincipal {
        RuntimePrincipal::AgentSlot {
            agent: agent("someone-else"),
            custody: RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        }
    }

    #[derive(Clone)]
    struct Custody {
        current: Arc<Mutex<Result<Option<RuntimePrincipal>, ()>>>,
    }
    impl Custody {
        /// No live AgentSlot custody — focus is never consulted.
        fn absent() -> Self {
            Self {
                current: Arc::new(Mutex::new(Ok(None))),
            }
        }
        fn current(principal: RuntimePrincipal) -> Self {
            Self {
                current: Arc::new(Mutex::new(Ok(Some(principal)))),
            }
        }
        fn faulting() -> Self {
            Self {
                current: Arc::new(Mutex::new(Err(()))),
            }
        }
    }
    impl RuntimeCustodyReadPort for Custody {
        async fn current(
            &self,
            _scope: RuntimeScope,
        ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
            match &*self.current.lock().unwrap() {
                Ok(principal) => Ok(principal.clone()),
                Err(()) => Err(boom()),
            }
        }
    }

    #[derive(Clone)]
    struct Focus {
        answer: Arc<Mutex<Result<Option<ConversationId>, ()>>>,
        calls: Arc<Mutex<usize>>,
    }
    impl Focus {
        fn build(answer: Result<Option<ConversationId>, ()>) -> Self {
            Self {
                answer: Arc::new(Mutex::new(answer)),
                calls: Arc::new(Mutex::new(0)),
            }
        }
        /// Never consulted (no live AgentSlot custody, or a rejected custody fact).
        fn unused() -> Self {
            Self::build(Ok(None))
        }
        fn presenting(conversation: ConversationId) -> Self {
            Self::build(Ok(Some(conversation)))
        }
        /// The custody read saw a live slot, but the focus port's re-check found it gone.
        fn expired() -> Self {
            Self::build(Ok(None))
        }
        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }
    impl AgentSlotPresentationFocusPort for Focus {
        async fn agent_slot_presentation_focus(
            &self,
            _principal: RuntimePrincipal,
        ) -> Result<Option<ConversationId>, PersistenceFailure> {
            *self.calls.lock().unwrap() += 1;
            match &*self.answer.lock().unwrap() {
                Ok(focus) => Ok(focus.clone()),
                Err(()) => Err(boom()),
            }
        }
    }

    // ── runtime-transition write fake ────────────────────────────────────

    fn faithful_after(now_ms: i64) -> RuntimeTransitionAfterImage {
        RuntimeTransitionAfterImage {
            agent_id: agent(AGENT),
            runtime: "codex".into(),
            model: Some("gpt".into()),
            reasoning_effort: None,
            service_tier: None,
            updated_at: now_ms,
            transition_target: None,
            transition_started_at: None,
        }
    }

    #[derive(Clone)]
    struct Transitions {
        begin: BeginTransitionRecord,
        complete: CompleteTransitionRecord,
        begins: Arc<Mutex<usize>>,
        aborts: Arc<Mutex<usize>>,
        completes: Arc<Mutex<usize>>,
    }
    impl Transitions {
        fn new(begin: BeginTransitionRecord, complete: CompleteTransitionRecord) -> Self {
            Self {
                begin,
                complete,
                begins: Arc::new(Mutex::new(0)),
                aborts: Arc::new(Mutex::new(0)),
                completes: Arc::new(Mutex::new(0)),
            }
        }
        /// A writer that locks and commits faithfully to `codex` at `NOW`.
        fn committing() -> Self {
            Self::new(
                BeginTransitionRecord::Locked(lock_row()),
                CompleteTransitionRecord::Committed(faithful_after(NOW)),
            )
        }
        fn begins(&self) -> usize {
            *self.begins.lock().unwrap()
        }
        fn aborts(&self) -> usize {
            *self.aborts.lock().unwrap()
        }
        fn completes(&self) -> usize {
            *self.completes.lock().unwrap()
        }
    }
    impl RuntimeTransitionWrite for Transitions {
        async fn begin(
            &self,
            _agent: &AgentId,
            _target: RuntimeKind,
            _now_ms: i64,
        ) -> Result<BeginTransitionRecord, PersistenceFailure> {
            *self.begins.lock().unwrap() += 1;
            Ok(self.begin.clone())
        }
        async fn abort(
            &self,
            _lock: &RuntimeTransitionLock,
        ) -> Result<AbortTransition, PersistenceFailure> {
            *self.aborts.lock().unwrap() += 1;
            Ok(AbortTransition::Released)
        }
        async fn complete(
            &self,
            _lock: &RuntimeTransitionLock,
            _commit: &RuntimeTransitionCommit,
            _now_ms: i64,
        ) -> Result<CompleteTransitionRecord, PersistenceFailure> {
            *self.completes.lock().unwrap() += 1;
            Ok(self.complete.clone())
        }
    }

    // ── runtime-control provider fake ────────────────────────────────────

    #[derive(Clone, Copy)]
    enum ClearScript {
        Ok,
        Refuse,
        Transport,
    }

    #[derive(Clone)]
    struct Control {
        clears: Arc<Mutex<VecDeque<ClearScript>>>,
        /// The scope of every clear_session call, in order.
        cleared: Arc<Mutex<Vec<RuntimeScope>>>,
        /// The presentation origin of every start call, in order.
        started: Arc<Mutex<Vec<ConversationId>>>,
    }
    #[derive(Debug)]
    struct NeverFails;
    impl std::fmt::Display for NeverFails {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("never fails")
        }
    }
    impl std::error::Error for NeverFails {}
    impl Control {
        fn new() -> Self {
            Self {
                clears: Arc::new(Mutex::new(VecDeque::new())),
                cleared: Arc::new(Mutex::new(Vec::new())),
                started: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn scripting(scripts: Vec<ClearScript>) -> Self {
            let control = Self::new();
            *control.clears.lock().unwrap() = scripts.into();
            control
        }
        fn cleared(&self) -> Vec<RuntimeScope> {
            self.cleared.lock().unwrap().clone()
        }
        fn started(&self) -> Vec<ConversationId> {
            self.started.lock().unwrap().clone()
        }
    }
    impl RuntimeControlPort for Control {
        type Error = NeverFails;

        async fn start(
            &self,
            target: &RuntimeStartTarget,
        ) -> Result<Result<StartRuntimeOutcome, StartRuntimeProviderRefusal>, Self::Error> {
            assert_eq!(target.actor().as_str(), "user-1");
            assert_eq!(target.agent().as_str(), "agent-1");
            self.started
                .lock()
                .unwrap()
                .push(target.presentation_origin().clone());
            Ok(Ok(StartRuntimeOutcome))
        }
        async fn hard_stop(
            &self,
            _target: &RuntimeSeatTarget,
        ) -> Result<
            Result<
                crate::conversation_messaging::messaging::collaboration::HardStopRuntimeOutcome,
                crate::conversation_messaging::messaging::collaboration::HardStopRuntimeProviderRefusal,
            >,
            Self::Error,
        >{
            unreachable!("switch saga never hard-stops")
        }
        async fn clear_session(
            &self,
            target: &RuntimeSeatTarget,
        ) -> Result<Result<ClearSessionOutcome, ClearSessionProviderRefusal>, Self::Error> {
            assert_eq!(target.actor().as_str(), "user-1");
            assert_eq!(target.agent().as_str(), "agent-1");
            self.cleared.lock().unwrap().push(target.scope().clone());
            let script = self
                .clears
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ClearScript::Ok);
            Ok(match script {
                ClearScript::Ok => Ok(ClearSessionOutcome::committed(None, true)),
                ClearScript::Refuse => Err(ClearSessionProviderRefusal::Placement(
                    RuntimePlacementRefusal::HostMissing,
                )),
                ClearScript::Transport => return Err(NeverFails),
            })
        }
    }

    // ── agent_changed sink fake ──────────────────────────────────────────

    /// Records every `agent_changed` publish, in order; `fail` makes the announce error so a test
    /// can prove the committed switch still stands when the best-effort hint fails.
    #[derive(Clone)]
    struct ChangedSink {
        announced: Arc<Mutex<Vec<AgentId>>>,
        fail: bool,
    }
    impl ChangedSink {
        fn ok() -> Self {
            Self {
                announced: Arc::new(Mutex::new(Vec::new())),
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                announced: Arc::new(Mutex::new(Vec::new())),
                fail: true,
            }
        }
        fn announced(&self) -> Vec<AgentId> {
            self.announced.lock().unwrap().clone()
        }
    }
    impl AgentChangedSink for ChangedSink {
        async fn agent_changed(
            &self,
            agent: &AgentId,
        ) -> Result<(), crate::identity_access::agent_profile_write::AgentChangedAnnounceFailure>
        {
            self.announced.lock().unwrap().push(agent.clone());
            if self.fail {
                Err(
                    crate::identity_access::agent_profile_write::AgentChangedAnnounceFailure::from_source(
                        NeverFails,
                    ),
                )
            } else {
                Ok(())
            }
        }
    }

    // ── concurrency discriminator fake ───────────────────────────────────

    /// Enter one phase's critical section: bump the in-flight count, record the running peak,
    /// then park exactly once on `yield_now` before releasing. The guards are dropped at the end
    /// of each block, never held across the await, so a serial caller sees a peak of 1 while the
    /// concurrent `join_all` lets every sibling enter before any resumes and the peak reaches the
    /// call count. Determinism rides `yield_now`, never a wall-clock sleep.
    async fn gated(active: &Mutex<usize>, peak: &Mutex<usize>) {
        {
            let mut running = active.lock().unwrap();
            *running += 1;
            let mut high_water = peak.lock().unwrap();
            *high_water = (*high_water).max(*running);
        }
        tokio::task::yield_now().await;
        {
            let mut running = active.lock().unwrap();
            *running -= 1;
        }
    }

    /// A control port that yields inside every clear and start so a fan-out is observable, tracking
    /// the peak simultaneous in-flight calls per phase. All calls succeed; the test reads only the
    /// peaks to tell concurrent fan-out (peak == seat/target count) from a serial loop (peak == 1).
    #[derive(Clone)]
    struct GatedControl {
        clear_active: Arc<Mutex<usize>>,
        clear_peak: Arc<Mutex<usize>>,
        start_active: Arc<Mutex<usize>>,
        start_peak: Arc<Mutex<usize>>,
    }
    impl GatedControl {
        fn new() -> Self {
            Self {
                clear_active: Arc::new(Mutex::new(0)),
                clear_peak: Arc::new(Mutex::new(0)),
                start_active: Arc::new(Mutex::new(0)),
                start_peak: Arc::new(Mutex::new(0)),
            }
        }
        fn clear_peak(&self) -> usize {
            *self.clear_peak.lock().unwrap()
        }
        fn start_peak(&self) -> usize {
            *self.start_peak.lock().unwrap()
        }
    }
    impl RuntimeControlPort for GatedControl {
        type Error = NeverFails;

        async fn start(
            &self,
            _target: &RuntimeStartTarget,
        ) -> Result<Result<StartRuntimeOutcome, StartRuntimeProviderRefusal>, Self::Error> {
            gated(&self.start_active, &self.start_peak).await;
            Ok(Ok(StartRuntimeOutcome))
        }
        async fn hard_stop(
            &self,
            _target: &RuntimeSeatTarget,
        ) -> Result<
            Result<
                crate::conversation_messaging::messaging::collaboration::HardStopRuntimeOutcome,
                crate::conversation_messaging::messaging::collaboration::HardStopRuntimeProviderRefusal,
            >,
            Self::Error,
        >{
            unreachable!("switch saga never hard-stops")
        }
        async fn clear_session(
            &self,
            _target: &RuntimeSeatTarget,
        ) -> Result<Result<ClearSessionOutcome, ClearSessionProviderRefusal>, Self::Error> {
            gated(&self.clear_active, &self.clear_peak).await;
            Ok(Ok(ClearSessionOutcome::committed(None, true)))
        }
    }

    // ── the assembled saga call ──────────────────────────────────────────

    struct Harness {
        reads: SnapshotReads,
        probe: Probe,
        coordination: Coordination,
        transitions: Transitions,
        facts: TransitionFacts,
        custody: Custody,
        focus: Focus,
        control: Control,
    }
    impl Harness {
        async fn run(
            &self,
            request: &SwitchAgentRuntimeRequest,
        ) -> Result<SwitchAgentRuntimeCommitted, SwitchAgentRuntimeError> {
            self.run_with_changed(request, &ChangedSink::ok()).await
        }

        /// Same as [`run`], but with a caller-held sink so a test can assert the post-commit
        /// `agent_changed` publish count and that a failing sink never flips the committed result.
        /// Drops the carried announce fate (see [`run_announced`] to inspect it) after consuming
        /// the committed tallies through `into_parts`.
        async fn run_with_changed(
            &self,
            request: &SwitchAgentRuntimeRequest,
            changed: &ChangedSink,
        ) -> Result<SwitchAgentRuntimeCommitted, SwitchAgentRuntimeError> {
            self.run_announced(request, changed)
                .await
                .map(|announced| announced.into_parts().0)
        }

        /// The raw outer outcome, so a test can assert the carried `agent_changed` announce fate
        /// (`Delivered` / `Failed`) that the boundary must log — not just that the committed result
        /// never flips.
        async fn run_announced(
            &self,
            request: &SwitchAgentRuntimeRequest,
            changed: &ChangedSink,
        ) -> Result<AnnouncedRuntimeSwitch, SwitchAgentRuntimeError> {
            switch_agent_runtime(
                &self.reads,
                &self.probe,
                &self.coordination,
                &self.transitions,
                &self.facts,
                &self.facts,
                &self.custody,
                &self.focus,
                &self.control,
                changed,
                request,
                NOW,
            )
            .await
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Preflight refusals never touch the lock or any effect.
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn unchanged_runtime_refuses_before_the_lock() {
        let (req, _from) = request(RuntimeKind::Codex, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(RuntimeKind::Codex, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::empty(),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::RuntimeUnchanged)
        );
        assert_eq!(harness.transitions.begins(), 0, "no lock acquired");
        assert!(harness.control.cleared().is_empty(), "no session cleared");
        assert_eq!(harness.facts.reads(), 0, "no seat read before the lock");
        assert_eq!(
            harness.probe.calls(),
            0,
            "unchanged short-circuits before probe"
        );
    }

    #[tokio::test]
    async fn a_non_selectable_runtime_refuses_before_the_lock() {
        // Cursor is a real RuntimeKind that is NOT in SELECTABLE_RUNTIMES.
        let (req, from) = request(RuntimeKind::Codex, RuntimeKind::Cursor);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::empty(),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::RuntimeNotSelectable)
        );
        assert_eq!(harness.transitions.begins(), 0, "no lock acquired");
        assert_eq!(
            harness.probe.calls(),
            0,
            "unselectable short-circuits before the probe"
        );
    }

    #[tokio::test]
    async fn deterministically_absent_runtime_refuses_before_the_lock() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::new(Ok(RuntimeAvailabilityVerdict::Absent)),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::empty(),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::RuntimeNotInstalled)
        );
        assert_eq!(
            harness.transitions.begins(),
            0,
            "absent probe blocks before the lock"
        );
        assert!(harness.control.cleared().is_empty());
    }

    #[tokio::test]
    async fn probe_fault_surfaces_and_never_takes_the_lock() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::new(Err(RuntimeAvailabilityProbeFault::AttachPending)),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::empty(),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::Availability(
                RuntimeAvailabilityProbeFault::AttachPending
            ))
        );
        assert_eq!(harness.transitions.begins(), 0);
    }

    #[tokio::test]
    async fn missing_agent_refuses() {
        let (req, _from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::scripted(vec![Ok(None)]),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::empty(),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::AgentNotFound)
        );
        assert_eq!(harness.probe.calls(), 0);
    }

    #[tokio::test]
    async fn a_snapshot_from_a_foreign_row_is_a_fault_not_an_answer() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let mut foreign = snapshot(from, AgentSessionScope::Agent);
        foreign.source = agent("someone-else");
        let harness = Harness {
            reads: SnapshotReads::scripted(vec![Ok(Some(foreign))]),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::empty(),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::Fault(
                SwitchAgentRuntimeFault::SourceUntrustworthy
            ))
        );
        assert_eq!(harness.transitions.begins(), 0);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Lock-acquisition outcomes.
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_in_progress_transition_refuses_before_reading_seats() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::new(
                BeginTransitionRecord::InProgress,
                CompleteTransitionRecord::Lost,
            ),
            facts: TransitionFacts::empty(),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::TransitionInProgress)
        );
        assert!(
            harness.control.cleared().is_empty(),
            "a lost lock clears nothing"
        );
        assert_eq!(harness.facts.reads(), 0, "a lost lock reads no seats");
    }

    #[tokio::test]
    async fn a_deleting_agent_refuses_the_transition() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::new(
                BeginTransitionRecord::Deleting,
                CompleteTransitionRecord::Lost,
            ),
            facts: TransitionFacts::empty(),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::DeletionInProgress)
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Under the lock: re-read staleness, sealed seats, clear-all, commit, re-drive.
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_target_reached_by_a_racing_commit_is_caught_on_the_under_lock_reread() {
        // Preflight sees claude→codex; by the time the lock is held another request has
        // already committed codex, so the re-read must refuse rather than overwrite tuning.
        let (req, _from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::scripted(vec![
                Ok(Some(snapshot(
                    RuntimeKind::Claude,
                    AgentSessionScope::Agent,
                ))),
                Ok(Some(snapshot(RuntimeKind::Codex, AgentSessionScope::Agent))),
            ]),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::empty(),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::RuntimeUnchanged)
        );
        assert_eq!(harness.reads.calls(), 2, "preflight + under-lock re-read");
        assert_eq!(harness.transitions.begins(), 1, "the lock was taken");
        assert_eq!(harness.transitions.aborts(), 1, "then aborted");
        assert_eq!(harness.transitions.completes(), 0, "never committed");
        assert_eq!(
            harness.facts.reads(),
            0,
            "a stale re-read refuses before the seat read"
        );
    }

    #[tokio::test]
    async fn a_foreign_seat_snapshot_is_a_fault_and_clears_nothing() {
        // The seat read returns a snapshot naming a seat owned by another agent: the sealed
        // read rejects it, so the saga faults and clears nothing (P0-1 falsifier).
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let foreign_seat = RuntimeTransitionSeatRow {
            scope: RuntimeScope::ConversationSlot {
                agent: agent("someone-else"),
                source: conv("c-a"),
            },
            observed_status: RuntimeObservedStatus::Active,
            cli_session_id: Some("cli".into()),
        };
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Conversation)),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![foreign_seat]),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::Fault(
                SwitchAgentRuntimeFault::Transition
            ))
        );
        assert!(
            harness.control.cleared().is_empty(),
            "a rejected seat snapshot clears nothing"
        );
        assert_eq!(harness.transitions.completes(), 0, "never committed");
        assert_eq!(harness.transitions.aborts(), 1, "lock aborted");
    }

    #[tokio::test]
    async fn conversation_scope_clears_every_live_or_resumable_seat_then_commits_and_redrives() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let seats = vec![
            conv_seat("c-active", RuntimeObservedStatus::Active, true),
            conv_seat("c-starting", RuntimeObservedStatus::Starting, true),
            conv_seat("c-resumable", RuntimeObservedStatus::Idle, true),
            // Idle with no resumable session: nothing to clear, nothing to restart.
            conv_seat("c-dead", RuntimeObservedStatus::Idle, false),
        ];
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Conversation)),
            probe: Probe::available(),
            coordination: Coordination {
                pending_since: vec![conv("c-active"), conv("c-late")],
                ..Coordination::default()
            },
            transitions: Transitions::committing(),
            facts: TransitionFacts::new(seats, vec![conv("c-reclaimed")]),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        let committed = harness.run(&req).await.expect("commit");

        // Every live-or-resumable seat cleared; the dead one skipped.
        assert_eq!(committed.cleared_sessions, 3);
        assert_eq!(
            harness.control.cleared(),
            vec![
                RuntimeScope::ConversationSlot {
                    agent: agent("agent-1"),
                    source: conv("c-active")
                },
                RuntimeScope::ConversationSlot {
                    agent: agent("agent-1"),
                    source: conv("c-starting")
                },
                RuntimeScope::ConversationSlot {
                    agent: agent("agent-1"),
                    source: conv("c-resumable")
                },
            ]
        );
        assert_eq!(harness.transitions.completes(), 1);
        assert_eq!(harness.transitions.aborts(), 0);
        assert_eq!(*harness.facts.reclaims.lock().unwrap(), 1);

        // Re-drive is live-before ∪ reclaimed ∪ newly-pending, deduped, insertion-ordered.
        // Only the two live conversations were live-before (resumable-idle is not restarted).
        assert_eq!(
            harness.control.started(),
            vec![
                conv("c-active"),
                conv("c-starting"),
                conv("c-reclaimed"),
                conv("c-late"),
            ]
        );
        assert_eq!(committed.restarted_conversations, 4);
        assert_eq!(committed.restart_failures, 0);
    }

    #[tokio::test]
    async fn clear_failures_attempt_every_seat_abort_and_restore_without_announcing() {
        for failure in [ClearScript::Refuse, ClearScript::Transport] {
            let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
            let harness = Harness {
                reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Conversation)),
                probe: Probe::available(),
                coordination: Coordination::default(),
                transitions: Transitions::committing(),
                facts: TransitionFacts::with_seats(vec![
                    conv_seat("c-a", RuntimeObservedStatus::Active, true),
                    conv_seat("c-b", RuntimeObservedStatus::Active, true),
                ]),
                custody: Custody::absent(),
                focus: Focus::unused(),
                // Fail first so an early return cannot masquerade as attempting every seat.
                control: Control::scripting(vec![failure, ClearScript::Ok]),
            };
            let sink = ChangedSink::ok();
            assert_eq!(
                harness.run_with_changed(&req, &sink).await,
                Err(SwitchAgentRuntimeError::SessionClearFailed)
            );
            assert_eq!(
                harness.control.cleared(),
                vec![
                    RuntimeScope::ConversationSlot {
                        agent: agent("agent-1"),
                        source: conv("c-a")
                    },
                    RuntimeScope::ConversationSlot {
                        agent: agent("agent-1"),
                        source: conv("c-b")
                    },
                ]
            );
            assert_eq!(harness.transitions.completes(), 0);
            assert_eq!(harness.transitions.aborts(), 1);
            assert_eq!(harness.control.started(), vec![conv("c-a"), conv("c-b")]);
            assert!(sink.announced().is_empty(), "no commit, no hint");
        }
    }

    #[tokio::test]
    async fn a_lost_commit_leaves_cleared_sessions_and_fires_no_restore() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Conversation)),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::new(
                BeginTransitionRecord::Locked(lock_row()),
                CompleteTransitionRecord::Lost,
            ),
            facts: TransitionFacts::with_seats(vec![conv_seat(
                "c-a",
                RuntimeObservedStatus::Active,
                true,
            )]),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        let sink = ChangedSink::ok();
        assert_eq!(
            harness.run_with_changed(&req, &sink).await,
            Err(SwitchAgentRuntimeError::TransitionLost)
        );
        assert!(sink.announced().is_empty(), "no commit, no hint");
        assert_eq!(
            harness.control.cleared().len(),
            1,
            "the seat was cleared before the lost commit"
        );
        assert!(
            harness.control.started().is_empty(),
            "a lost commit restores nothing"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Agent-scope re-drive: one restart target only.
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_scope_restarts_only_the_oldest_pending_conversation() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination {
                pending: vec![conv("oldest"), conv("newer")],
                pending_since: vec![conv("late")],
                ..Coordination::default()
            },
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![agent_seat(
                RuntimeObservedStatus::Active,
                false,
            )]),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        let committed = harness.run(&req).await.expect("commit");
        assert_eq!(committed.cleared_sessions, 1);
        assert_eq!(
            harness.control.cleared(),
            vec![RuntimeScope::AgentSlot {
                agent: agent("agent-1")
            }]
        );
        assert_eq!(
            harness.control.started(),
            vec![conv("oldest")],
            "one AgentSlot restart at the oldest pending origin"
        );
        assert_eq!(committed.restarted_conversations, 1);
    }

    #[tokio::test]
    async fn agent_scope_reuses_the_pre_clear_focus_only_while_still_a_participant() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        // No pending conversations, but the agent seat was live and the focus captured before
        // clear (via the custody→focus ports) is still a participant → resume at that focus.
        let live_participant = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination {
                participant: true,
                ..Coordination::default()
            },
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![agent_seat(
                RuntimeObservedStatus::Active,
                false,
            )]),
            custody: Custody::current(agent_principal()),
            focus: Focus::presenting(conv("focus")),
            control: Control::new(),
        };
        let committed = live_participant.run(&req).await.expect("commit");
        assert_eq!(
            live_participant.control.started(),
            vec![conv("focus")],
            "focus reused while participation holds"
        );
        assert_eq!(committed.restarted_conversations, 1);

        // Same shape but the focus is no longer a participant → nothing to restart.
        let gone = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination {
                participant: false,
                ..Coordination::default()
            },
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![agent_seat(
                RuntimeObservedStatus::Active,
                false,
            )]),
            custody: Custody::current(agent_principal()),
            focus: Focus::presenting(conv("focus")),
            control: Control::new(),
        };
        let committed = gone.run(&req).await.expect("commit");
        assert!(
            gone.control.started().is_empty(),
            "a stale focus is never restarted"
        );
        assert_eq!(committed.restarted_conversations, 0);
    }

    #[tokio::test]
    async fn absent_focus_after_current_custody_read_degrades_to_no_restart() {
        // The fake supplies current custody followed by no focus. The saga must keep
        // clearing and committing without inventing a restart origin.
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination {
                participant: true,
                ..Coordination::default()
            },
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![agent_seat(
                RuntimeObservedStatus::Active,
                false,
            )]),
            custody: Custody::current(agent_principal()),
            focus: Focus::expired(),
            control: Control::new(),
        };
        let committed = harness
            .run(&req)
            .await
            .expect("commit despite the expired focus");
        assert_eq!(
            committed.cleared_sessions, 1,
            "the live seat was still cleared"
        );
        assert_eq!(harness.transitions.completes(), 1);
        assert!(
            harness.control.started().is_empty(),
            "an expired focus resumes nothing — never a successor's focus"
        );
        assert_eq!(committed.restart_failures, 0);
    }

    #[tokio::test]
    async fn agent_scope_focus_capture_fault_fails_closed_before_any_clear() {
        // A persistence fault on the pre-clear custody read fails closed BEFORE any clear or
        // commit — the runtime is never changed.
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![agent_seat(
                RuntimeObservedStatus::Active,
                false,
            )]),
            custody: Custody::faulting(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::Fault(
                SwitchAgentRuntimeFault::Persistence
            ))
        );
        assert!(
            harness.control.cleared().is_empty(),
            "a focus-capture fault clears nothing"
        );
        assert_eq!(harness.transitions.completes(), 0, "never committed");
        assert_eq!(harness.transitions.aborts(), 1, "lock aborted");
    }

    #[tokio::test]
    async fn a_custody_current_for_a_foreign_agent_is_a_fault_and_never_becomes_this_agents_focus()
    {
        // Switch agent A; the custody port answers A's AgentSlot `current` read with B's live
        // principal. Core must source-bind the raw fact and reject it as an internal witness
        // fault — never hand B's principal to the focus port, and never clear or commit (P0-1).
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let focus = Focus::presenting(conv("b-focus"));
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination {
                participant: true,
                ..Coordination::default()
            },
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![agent_seat(
                RuntimeObservedStatus::Active,
                false,
            )]),
            custody: Custody::current(other_agent_principal()),
            focus: focus.clone(),
            control: Control::new(),
        };
        assert_eq!(
            harness.run(&req).await,
            Err(SwitchAgentRuntimeError::Fault(
                SwitchAgentRuntimeFault::CustodyScopeMismatch
            ))
        );
        assert_eq!(
            focus.calls(),
            0,
            "a foreign custody fact is never handed to the focus port"
        );
        assert!(
            harness.control.cleared().is_empty(),
            "a rejected custody fact clears nothing"
        );
        assert_eq!(harness.transitions.completes(), 0, "never committed");
        assert_eq!(harness.transitions.aborts(), 1, "lock aborted");
    }

    #[tokio::test]
    async fn agent_scope_with_nothing_live_and_no_pending_restarts_nothing() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination {
                participant: true,
                ..Coordination::default()
            },
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![agent_seat(
                RuntimeObservedStatus::Idle,
                false,
            )]),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        let committed = harness.run(&req).await.expect("commit");
        assert_eq!(
            committed.cleared_sessions, 0,
            "an idle agent seat holds no session"
        );
        assert!(harness.control.started().is_empty());
        assert_eq!(committed.restarted_conversations, 0);
    }

    #[tokio::test]
    async fn conversation_scope_dispatches_known_live_even_when_the_supplemental_read_faults() {
        // The switch commits; the KNOWN live conversation (live_before) is dispatched regardless,
        // and the post-commit newly-pending read faults. The committed transition stands (Ok),
        // c-a is still restarted, and the fault is surfaced in `resume_read_faults` — never a
        // silent 0 and never dragging the known candidate down with it.
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Conversation)),
            probe: Probe::available(),
            coordination: Coordination {
                fault_pending_since: true,
                ..Coordination::default()
            },
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![conv_seat(
                "c-a",
                RuntimeObservedStatus::Active,
                true,
            )]),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        let committed = harness
            .run(&req)
            .await
            .expect("a post-commit read fault never reverts the commit");
        assert_eq!(harness.transitions.completes(), 1, "committed");
        assert_eq!(
            harness.transitions.aborts(),
            0,
            "never aborted after commit"
        );
        assert_eq!(committed.cleared_sessions, 1);
        assert_eq!(
            harness.control.started(),
            vec![conv("c-a")],
            "the known live conversation is dispatched despite the faulted supplemental read"
        );
        assert_eq!(committed.restarted_conversations, 1);
        assert_eq!(committed.restart_failures, 0);
        assert_eq!(
            committed.resume_read_faults, 1,
            "the aftermath fault is surfaced, never a silent 0"
        );
    }

    #[tokio::test]
    async fn agent_scope_falls_back_to_focus_when_the_oldest_pending_read_faults() {
        // AgentSlot was live and a focus F was captured pre-clear; the oldest-pending read
        // faults. The saga must still try F when participation holds, and surface the fault.
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Agent)),
            probe: Probe::available(),
            coordination: Coordination {
                participant: true,
                fault_pending: true,
                ..Coordination::default()
            },
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![agent_seat(
                RuntimeObservedStatus::Active,
                false,
            )]),
            custody: Custody::current(agent_principal()),
            focus: Focus::presenting(conv("focus")),
            control: Control::new(),
        };
        let committed = harness.run(&req).await.expect("commit");
        assert_eq!(
            harness.control.started(),
            vec![conv("focus")],
            "focus fallback still attempted after the oldest-pending read faulted"
        );
        assert_eq!(committed.restarted_conversations, 1);
        assert_eq!(
            committed.resume_read_faults, 1,
            "the faulted oldest-pending read is surfaced"
        );
    }

    #[tokio::test]
    async fn a_restart_failure_after_commit_is_counted_never_rolled_back() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        // A committing writer; the control port refuses the restart. The transition stays
        // committed and the failure is only counted.
        #[derive(Clone)]
        struct RefusingStart(Control);
        impl RuntimeControlPort for RefusingStart {
            type Error = NeverFails;
            async fn start(
                &self,
                _target: &RuntimeStartTarget,
            ) -> Result<Result<StartRuntimeOutcome, StartRuntimeProviderRefusal>, Self::Error>
            {
                Ok(Err(StartRuntimeProviderRefusal::ShuttingDown))
            }
            async fn hard_stop(
                &self,
                _target: &RuntimeSeatTarget,
            ) -> Result<
                Result<
                    crate::conversation_messaging::messaging::collaboration::HardStopRuntimeOutcome,
                    crate::conversation_messaging::messaging::collaboration::HardStopRuntimeProviderRefusal,
                >,
                Self::Error,
            >{
                unreachable!()
            }
            async fn clear_session(
                &self,
                target: &RuntimeSeatTarget,
            ) -> Result<Result<ClearSessionOutcome, ClearSessionProviderRefusal>, Self::Error>
            {
                self.0.clear_session(target).await
            }
        }
        let facts = TransitionFacts::with_seats(vec![conv_seat(
            "c-a",
            RuntimeObservedStatus::Active,
            true,
        )]);
        let committed = switch_agent_runtime(
            &SnapshotReads::stable(snapshot(from, AgentSessionScope::Conversation)),
            &Probe::available(),
            &Coordination::default(),
            &Transitions::committing(),
            &facts,
            &facts,
            &Custody::absent(),
            &Focus::unused(),
            &RefusingStart(Control::new()),
            &ChangedSink::ok(),
            &req,
            NOW,
        )
        .await
        .expect("transition still commits despite restart refusal")
        .into_parts()
        .0;
        assert_eq!(committed.restarted_conversations, 0);
        assert_eq!(
            committed.restart_failures, 1,
            "the refusal is counted, not rolled back"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // The committed change publishes `agent_changed` exactly once, keyed on the
    // sealed committed id, best-effort — after commit, before the restart aftermath.
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_committed_switch_announces_agent_changed_once_keyed_on_the_committed_agent() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Conversation)),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![conv_seat(
                "c-a",
                RuntimeObservedStatus::Active,
                true,
            )]),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        let sink = ChangedSink::ok();
        let (_committed, announce) = harness
            .run_announced(&req, &sink)
            .await
            .expect("commit")
            .into_parts();
        assert!(
            matches!(announce, AgentChangedAnnounce::Delivered),
            "a delivered hint is carried as Delivered, not dropped"
        );
        assert_eq!(
            sink.announced(),
            vec![agent(AGENT)],
            "exactly one hint, keyed on the committed agent id"
        );
    }

    #[tokio::test]
    async fn a_failing_agent_changed_sink_never_flips_the_committed_switch() {
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let harness = Harness {
            reads: SnapshotReads::stable(snapshot(from, AgentSessionScope::Conversation)),
            probe: Probe::available(),
            coordination: Coordination::default(),
            transitions: Transitions::committing(),
            facts: TransitionFacts::with_seats(vec![conv_seat(
                "c-a",
                RuntimeObservedStatus::Active,
                true,
            )]),
            custody: Custody::absent(),
            focus: Focus::unused(),
            control: Control::new(),
        };
        let sink = ChangedSink::failing();
        let (committed, announce) = harness
            .run_announced(&req, &sink)
            .await
            .expect("the committed switch stands despite a failing best-effort hint")
            .into_parts();
        assert_eq!(committed.cleared_sessions, 1);
        assert!(
            matches!(announce, AgentChangedAnnounce::Failed(_)),
            "the failed announce fate is CARRIED to the boundary, not swallowed"
        );
        assert_eq!(
            sink.announced(),
            vec![agent(AGENT)],
            "the hint was still attempted exactly once"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Clears (pre-commit) and restarts (post-commit) fan out concurrently, not serially.
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn clears_and_restarts_fan_out_concurrently_not_serially() {
        // Two live conversation seats → two clears pre-commit and two restarts post-commit. Under
        // the `join_all` fan-out both siblings enter their critical section before either resumes,
        // so each phase's peak in-flight count reaches 2; a serial loop would cap it at 1.
        let (req, from) = request(RuntimeKind::Claude, RuntimeKind::Codex);
        let facts = TransitionFacts::with_seats(vec![
            conv_seat("c-a", RuntimeObservedStatus::Active, true),
            conv_seat("c-b", RuntimeObservedStatus::Active, true),
        ]);
        let control = GatedControl::new();
        let committed = switch_agent_runtime(
            &SnapshotReads::stable(snapshot(from, AgentSessionScope::Conversation)),
            &Probe::available(),
            &Coordination::default(),
            &Transitions::committing(),
            &facts,
            &facts,
            &Custody::absent(),
            &Focus::unused(),
            &control,
            &ChangedSink::ok(),
            &req,
            NOW,
        )
        .await
        .expect("commit")
        .into_parts()
        .0;
        assert_eq!(committed.cleared_sessions, 2);
        assert_eq!(committed.restarted_conversations, 2);
        assert_eq!(
            control.clear_peak(),
            2,
            "both clears were in-flight simultaneously, not serialized"
        );
        assert_eq!(
            control.start_peak(),
            2,
            "both restarts were in-flight simultaneously, not serialized"
        );
    }
}
