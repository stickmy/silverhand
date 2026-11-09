//! The task `status` verb's transition policy and the `closed_at` coupling.
//!
//! The two doors differ, and the difference is a real contract, not an
//! oversight:
//! - the AGENT door ([`agent_status_change`]) is gated by an explicit transition
//!   table; that table never lists a status as its own successor, so a
//!   same-status request is an `InvalidTransition` with no separate "already
//!   there" code.
//! - the HUMAN door ([`human_status_change`]) has no table: any target other
//!   than the current status is accepted, and only a same-status request is
//!   refused, as `AlreadyAtStatus`.
//!
//! They are separate entry points rather than one function taking an actor
//! selector because the door is never data — a route knows statically which one
//! it is — and a caller-settable selector only creates a way to plumb the wrong
//! policy in from a request.
//!
//! `closed_at` is coupled to the resulting status
//! (`(status = 'closed') = (closed_at IS NOT NULL)`): a task moving to `closed`
//! keeps its original close instant across a re-close, or is stamped `now` if it
//! had none; every other status clears it. What this slice proves today is
//! narrow and exact: legality and `closed_at` are decided in one step and leave
//! together inside one [`TaskStatusChange`], whose fields are private and
//! mintable only by the two planners — so a `(target, closed_at)` pair cannot be
//! forged from parts outside this module, and the two halves are always
//! same-source. A `compile_fail` gate plus a same-planner positive control keep
//! that boundary permanent, not merely true while the fields happen to be
//! private.
//!
//! What this slice does NOT yet prove — and the type therefore does not assert —
//! is that a write actually consumes the witness. `TaskStatus` is public and
//! `TaskStatusChange` exposes `target()` / `closed_at()`, so nothing here stops
//! a future consumer from reading the two out and binding them separately. "The
//! write path only accepts the witness / a status cannot be written without one
//! / the two binds cannot be dropped or bypassed" is an obligation on the
//! consumer-defined write port in a later slice: its signature must take a
//! `TaskStatusChange` (not a bare status + instant), and its real write test
//! must assert `status` and `closed_at` land together. This core seal does not
//! stand in for that gate.
//!
//! Authorization (assignee / creator-self-stale) and persistence remain the
//! caller's concern; this module is only the transition-legality and `closed_at`
//! decision.

use super::spec::{Assignee, Creator, TaskStatus};
use crate::foundation::identity::AgentId;

impl TaskStatus {
    /// The `status`-verb transitions the AGENT door permits from `self` — the
    /// `VALID_TRANSITIONS` table verbatim. Note `todo → in_progress` is absent:
    /// that edge is produced only by claim/dispatch, never this verb.
    ///
    /// Private on purpose: exposing the table would let a consumer check
    /// legality here and then write a `closed_at` decided somewhere else, which
    /// is exactly the split [`TaskStatusChange`] exists to prevent.
    fn agent_can_transition_to(self, target: TaskStatus) -> bool {
        use TaskStatus::*;
        match self {
            Todo => matches!(target, Closed),
            InProgress => matches!(target, InReview | Done | Closed),
            InReview => matches!(target, InProgress | Done | Closed),
            Done => matches!(target, Closed),
            Closed => matches!(target, Todo | InProgress | InReview | Done),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StatusTransitionRefusal {
    #[error("invalid transition {from:?} → {to:?}")]
    InvalidTransition { from: TaskStatus, to: TaskStatus },
    #[error("task already has that status")]
    AlreadyAtStatus,
}

/// Proof that a status change passed its door's transition gate, carrying the
/// resulting status together with the `closed_at` that status implies.
///
/// The fields are private and there is no public constructor: the two planners
/// are the sole mints, so a `(target, closed_at)` pair can only be one this
/// module computed as a unit. This is the same seal the edit witness carries,
/// and it is what the type asserts — a pair cannot be forged from parts, and its
/// two halves are same-source. It does NOT assert that a write consumes the
/// witness; `target()` / `closed_at()` are readable, and binding the two
/// separately is a shape the consumer-defined write port must foreclose (see the
/// module docs), not one this type prevents.
///
/// A witness cannot be fabricated from parts:
///
/// ```compile_fail
/// use silverhand_backend_core::conversation_messaging::messaging::task::TaskStatusChange;
/// use silverhand_backend_core::conversation_messaging::messaging::task::TaskStatus;
///
/// // fields `target` and `closed_at` are private
/// let forged = TaskStatusChange { target: TaskStatus::Closed, closed_at: Some(0) };
/// ```
///
/// The same path used positively, through the planner — so the failure above is
/// field privacy, not a stale import or a renamed type, and the accessors are
/// live:
///
/// ```
/// use silverhand_backend_core::conversation_messaging::messaging::task::{TaskStatus, agent_status_change};
///
/// let change = agent_status_change(TaskStatus::InProgress, TaskStatus::Closed, None, 42).unwrap();
/// assert_eq!(change.target(), TaskStatus::Closed);
/// assert_eq!(change.closed_at(), Some(42));
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskStatusChange {
    target: TaskStatus,
    closed_at: Option<i64>,
}

impl TaskStatusChange {
    pub fn target(self) -> TaskStatus {
        self.target
    }

    pub fn closed_at(self) -> Option<i64> {
        self.closed_at
    }
}

/// The AGENT `status` verb: gated by the transition table.
pub fn agent_status_change(
    from: TaskStatus,
    to: TaskStatus,
    current_closed_at: Option<i64>,
    now: i64,
) -> Result<TaskStatusChange, StatusTransitionRefusal> {
    if !from.agent_can_transition_to(to) {
        return Err(StatusTransitionRefusal::InvalidTransition { from, to });
    }
    Ok(change_to(to, current_closed_at, now))
}

/// The HUMAN `status` verb: no table, only a same-status refusal.
pub fn human_status_change(
    from: TaskStatus,
    to: TaskStatus,
    current_closed_at: Option<i64>,
    now: i64,
) -> Result<TaskStatusChange, StatusTransitionRefusal> {
    if from == to {
        return Err(StatusTransitionRefusal::AlreadyAtStatus);
    }
    Ok(change_to(to, current_closed_at, now))
}

/// A terminal task refuses assignment changes; the arm names which terminal status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AssignmentRefusal {
    #[error("task is done")]
    TaskDone,
    #[error("task is closed")]
    TaskClosed,
}

/// Whether the assignment may change at all; human unassign uses only this gate.
pub fn assignment_open(from: TaskStatus) -> Result<(), AssignmentRefusal> {
    match from {
        TaskStatus::Done => Err(AssignmentRefusal::TaskDone),
        TaskStatus::Closed => Err(AssignmentRefusal::TaskClosed),
        TaskStatus::Todo | TaskStatus::InProgress | TaskStatus::InReview => Ok(()),
    }
}

/// Claim or dispatch: the only producer of `todo → in_progress`; other open statuses
/// keep their status.
pub fn claim_status_change(from: TaskStatus) -> Result<TaskStatusChange, AssignmentRefusal> {
    assignment_open(from)?;
    let target = match from {
        TaskStatus::Todo => TaskStatus::InProgress,
        other => other,
    };
    Ok(TaskStatusChange {
        target,
        closed_at: None,
    })
}

/// Agent release: an open task rewinds to `todo`.
pub fn agent_release_status_change(
    from: TaskStatus,
) -> Result<TaskStatusChange, AssignmentRefusal> {
    assignment_open(from)?;
    Ok(TaskStatusChange {
        target: TaskStatus::Todo,
        closed_at: None,
    })
}

/// The `closed_at` a status change resolves to. Only the resulting status
/// matters: `closed` keeps an existing close instant or stamps `now` when there
/// was none; every other status clears it to `None`.
fn change_to(target: TaskStatus, current_closed_at: Option<i64>, now: i64) -> TaskStatusChange {
    let closed_at = match target {
        TaskStatus::Closed => Some(current_closed_at.unwrap_or(now)),
        _ => None,
    };
    TaskStatusChange { target, closed_at }
}

/// Why the AGENT `status` verb refused a task whose row was just read under the
/// write lock. The transition arm is the table gate; the authority arm is the
/// assignee/creator-self-stale gate. Kept distinct so the adapter maps each to its
/// own product code (`invalid_transition` / `not_assignee`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AgentStatusRefusal {
    #[error(transparent)]
    Transition(#[from] StatusTransitionRefusal),
    #[error(
        "only the assignee (or the creator of an unassigned self-created task) may update status"
    )]
    NotAssignee,
}

/// The AGENT `status` decision over a task's CURRENT row facts, decided as a unit
/// so a caller cannot authorize on one snapshot and transition on another. The
/// facts MUST be the ones read under the write lock (the row the write will touch),
/// because the whole point is that a concurrent claim/unclaim can move `assignee`
/// without moving `status`: authorizing on a pre-lock snapshot would let a stale
/// permission apply.
///
/// Permission mirrors `updateStatusAgent`: the actor is the assignee, OR the task
/// is unassigned (BOTH assignee columns empty) AND its creator is this same agent —
/// the creator-self-stale branch that lets an agent advance its own never-claimed
/// task. A human assignee or a different creator is refused before the transition
/// is even consulted.
pub fn agent_status_write_decision(
    actor: &AgentId,
    current_status: TaskStatus,
    assignee: &Assignee,
    creator: &Creator,
    current_closed_at: Option<i64>,
    target: TaskStatus,
    now: i64,
) -> Result<TaskStatusChange, AgentStatusRefusal> {
    let is_assignee_self = matches!(assignee, Assignee::Agent(agent) if agent == actor);
    let is_creator_self_stale = matches!(assignee, Assignee::Unassigned)
        && matches!(creator, Creator::Agent(agent) if agent == actor);
    if !is_assignee_self && !is_creator_self_stale {
        return Err(AgentStatusRefusal::NotAssignee);
    }
    agent_status_change(current_status, target, current_closed_at, now).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskStatus::*;

    fn allowed_agent_targets(from: TaskStatus) -> Vec<TaskStatus> {
        TaskStatus::ALL
            .into_iter()
            .filter(|&to| agent_status_change(from, to, None, 42).is_ok())
            .collect()
    }

    #[test]
    fn assignment_planners_promote_rewind_and_refuse_terminal() {
        let claimed = |from| claim_status_change(from).map(TaskStatusChange::target);
        assert_eq!(claimed(Todo), Ok(InProgress));
        assert_eq!(claimed(InProgress), Ok(InProgress));
        assert_eq!(claimed(InReview), Ok(InReview));
        assert_eq!(claimed(Done), Err(AssignmentRefusal::TaskDone));
        assert_eq!(claimed(Closed), Err(AssignmentRefusal::TaskClosed));
        let released = |from| agent_release_status_change(from).map(TaskStatusChange::target);
        assert_eq!(released(InReview), Ok(Todo));
        assert_eq!(released(Done), Err(AssignmentRefusal::TaskDone));
        assert_eq!(released(Closed), Err(AssignmentRefusal::TaskClosed));
        assert_eq!(claim_status_change(Todo).unwrap().closed_at(), None);
    }

    #[test]
    fn agent_planner_admits_exactly_the_valid_transitions() {
        assert_eq!(allowed_agent_targets(Todo), vec![Closed]);
        assert_eq!(
            allowed_agent_targets(InProgress),
            vec![InReview, Done, Closed]
        );
        assert_eq!(
            allowed_agent_targets(InReview),
            vec![InProgress, Done, Closed]
        );
        assert_eq!(allowed_agent_targets(Done), vec![Closed]);
        assert_eq!(
            allowed_agent_targets(Closed),
            vec![Todo, InProgress, InReview, Done]
        );
    }

    #[test]
    fn agent_door_maps_a_same_status_request_to_invalid_transition() {
        assert_eq!(
            agent_status_change(InProgress, InProgress, None, 42),
            Err(StatusTransitionRefusal::InvalidTransition {
                from: InProgress,
                to: InProgress,
            })
        );
        assert_eq!(
            agent_status_change(Todo, InProgress, None, 42),
            Err(StatusTransitionRefusal::InvalidTransition {
                from: Todo,
                to: InProgress,
            })
        );
        let change = agent_status_change(InProgress, Done, None, 42).unwrap();
        assert_eq!(change.target(), Done);
        assert_eq!(change.closed_at(), None);
    }

    #[test]
    fn human_door_has_no_table_and_only_refuses_a_same_status_request() {
        // Any target other than the current status is accepted — even edges the
        // agent table forbids (`todo → in_progress`, `closed → todo`).
        for (from, to) in [(Todo, InProgress), (Closed, Todo), (Done, Todo)] {
            let change = human_status_change(from, to, None, 42).unwrap();
            assert_eq!(change.target(), to);
        }
        assert_eq!(
            human_status_change(Done, Done, None, 42),
            Err(StatusTransitionRefusal::AlreadyAtStatus)
        );
    }

    #[test]
    fn closed_at_is_coupled_to_the_resulting_status() {
        // Moving to closed with no prior close instant stamps `now`.
        assert_eq!(
            agent_status_change(Done, Closed, None, 42)
                .unwrap()
                .closed_at(),
            Some(42)
        );
        // A re-close keeps the ORIGINAL close instant, not `now`.
        assert_eq!(
            human_status_change(Done, Closed, Some(7), 42)
                .unwrap()
                .closed_at(),
            Some(7)
        );
        // Any non-closed status clears it, whether or not one was set.
        assert_eq!(
            agent_status_change(Closed, InProgress, Some(7), 42)
                .unwrap()
                .closed_at(),
            None
        );
        assert_eq!(
            human_status_change(Todo, Done, None, 42)
                .unwrap()
                .closed_at(),
            None
        );
    }

    #[test]
    fn every_witness_either_door_can_mint_satisfies_the_column_check() {
        // The DDL invariant `(status = 'closed') = (closed_at IS NOT NULL)`,
        // asserted over the whole closed set rather than sampled — for both
        // doors and for a task that already carries a close instant or not.
        type Door = fn(
            TaskStatus,
            TaskStatus,
            Option<i64>,
            i64,
        ) -> Result<TaskStatusChange, StatusTransitionRefusal>;
        let doors: [(&str, Door); 2] = [
            ("agent", agent_status_change),
            ("human", human_status_change),
        ];
        let mut minted = 0;
        for (door, plan) in doors {
            for from in TaskStatus::ALL {
                for to in TaskStatus::ALL {
                    for current_closed_at in [None, Some(7)] {
                        let Ok(change) = plan(from, to, current_closed_at, 42) else {
                            continue; // refused: no witness exists to inspect
                        };
                        minted += 1;
                        assert_eq!(
                            change.closed_at().is_some(),
                            change.target() == Closed,
                            "{door}: {from:?} → {to:?} with {current_closed_at:?}"
                        );
                    }
                }
            }
        }
        // Legal agent edges (1 + 3 + 3 + 1 + 4 = 12) plus every human edge
        // (5 × 5 − 5 = 20), each over two prior `closed_at` values — a door that
        // silently stopped minting would not pass by vacuous truth.
        assert_eq!(minted, (12 + 20) * 2);
    }
}
