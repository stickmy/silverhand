//! The agent task board-list boundary: the one read route that projects an agent's visible
//! tasks, and the leaves it composes.
//!
//! - [`query`] — the `?workspace_id=…&status=…` grammar → the validated list coordinate.
//! - [`wire`] — the single summary projector, the parity of the TS `taskJoinedToSummary`.
//! - [`list`] — the route: resolve the opaque board scope, fetch, trim to the viewer, project.
//!
//! Like the send boundary, this crate defines the route but does not own its process: the
//! composition mounts [`list::task_list_router`] behind the runtime-credential authority and
//! supplies the store that answers the core's board/participation ports.

pub mod aftermath;
mod internal_detail;
mod internal_write;
mod list;
mod public_board;
mod public_point;
mod public_write;
mod query;
mod wire;
mod wire_number;

pub use aftermath::{
    AftermathFailure, AftermathOutcome, PublishingTaskAftermath, TaskAftermathActor,
    TaskAftermathTrigger, TaskCrossOperationAudit, TaskEffectExecute, TaskEffectPlan,
    TaskMutationAftermath, TaskSourcePlacementSnapshot,
};
pub use internal_detail::{TaskDetailState, task_detail_router};
pub use internal_write::assign::{TaskAssignState, task_assign_router};
pub use internal_write::claim::{TaskClaimState, task_claim_router};
pub use internal_write::create::{
    ResolvedPlacement, ResolvedTaskTarget, TaskCreatePairAllocation, TaskCreateState,
    TaskCreateTargetIntent, TaskCreateTargetResolver, TaskTargetRefusal, task_create_router,
};
pub use internal_write::edit::{TaskEditState, task_edit_router};
pub use internal_write::unclaim::{TaskUnclaimState, task_unclaim_router};
pub use internal_write::update_status::TaskNumberResolver;
pub use internal_write::update_status::{TaskStatusUpdateState, task_update_status_router};
pub use list::{TaskListState, task_list_router};
pub use public_board::{HumanTaskBoardState, human_task_board_router};
pub use public_point::{HumanTaskPointState, human_task_point_router};
pub use public_write::{
    HumanClaimState, HumanConversationPost, HumanCreateState, HumanEditState, HumanStatusState,
    human_claim_router, human_create_router, human_edit_router, human_status_router,
};
