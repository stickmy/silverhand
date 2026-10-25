//! Thin HTTP components for the residual workspace membership and capability faces.
//! The router is intentionally unmounted; backend-app supplies the concrete store later.

use axum::{
    Json, Router,
    extract::{Extension, FromRequest, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::membership::MembershipRuntimeStop,
    foundation::{
        identity::{AgentId, Principal, UserId, WorkspaceId},
        name_canonicalization::is_ecmascript_trim_whitespace,
    },
    identity_access::access::{
        AgentCapabilitySet, Capability, CapabilityReadOutcome, CapabilityReplaceOutcome,
        CapabilityReplacePreflight, HumanRoleChangeOutcome, OwnershipTransferOutcome,
        WorkspaceAgentRemovalOutcome, WorkspaceAgentRemovalPreflight, WorkspaceCapabilityPort,
        WorkspaceGovernanceFactsRead, WorkspaceGovernanceFloor, WorkspaceHumanRemovalOutcome,
        WorkspaceLeaveOutcome, WorkspaceMembershipAftermath, WorkspaceMembershipChanged,
        WorkspaceMembershipPort, WorkspaceOwnershipTransferPort, WorkspaceRole, WorkspaceRoster,
        WorkspaceRosterRead, WorkspaceViewerMembershipChanged, governance_floor_verdict,
    },
};

use crate::foundation::http_error::plain_500 as internal;
use crate::foundation::{error, wire_admission::ValidatedJson};

pub const MEMBERS_PATH: &str = "/api/workspaces/{id}/members";
pub const HUMAN_ROLE_PATH: &str = "/api/workspaces/{id}/members/users/{user_id}/role";
pub const AGENT_MEMBER_PATH: &str = "/api/workspaces/{id}/members/{agent_id}";
pub const HUMAN_MEMBER_PATH: &str = "/api/workspaces/{id}/members/users/{user_id}";
pub const LEAVE_PATH: &str = "/api/workspaces/{id}/leave";
pub const TRANSFER_OWNERSHIP_PATH: &str = "/api/workspaces/{id}/transfer-ownership";
pub const CAPABILITIES_PATH: &str = "/api/workspaces/{id}/agents/{agent_id}/capabilities";

#[derive(Clone)]
struct MembershipState<P, A> {
    port: P,
    aftermath: A,
}

/// Explicit component input for the seven residual workspace faces.  It merges
/// with the already-existing POST enrollment router only at backend-app.
pub fn router<P, A>(port: P, aftermath: A) -> Router
where
    P: WorkspaceMembershipPort
        + WorkspaceCapabilityPort
        + WorkspaceGovernanceFactsRead
        + Clone
        + Send
        + Sync
        + 'static,
    A: WorkspaceMembershipAftermath + MembershipRuntimeStop,
{
    Router::new()
        .route(MEMBERS_PATH, get(list::<P, A>))
        .route(HUMAN_ROLE_PATH, patch(change_role::<P, A>))
        .route(AGENT_MEMBER_PATH, delete(remove_agent::<P, A>))
        .route(HUMAN_MEMBER_PATH, delete(remove_human::<P, A>))
        .route(LEAVE_PATH, post(leave::<P, A>))
        .route(
            CAPABILITIES_PATH,
            get(read_capabilities::<P, A>).put(replace_capabilities::<P, A>),
        )
        .with_state(MembershipState { port, aftermath })
}

/// `POST /api/workspaces/{id}/transfer-ownership` `{"user_id"}` — one-step handover.
pub fn ownership_transfer_router<P, A>(port: P, aftermath: A) -> Router
where
    P: WorkspaceOwnershipTransferPort,
    A: WorkspaceMembershipAftermath,
{
    Router::new()
        .route(TRANSFER_OWNERSHIP_PATH, post(transfer_ownership::<P, A>))
        .with_state(TransferState { port, aftermath })
}

#[derive(Clone)]
struct TransferState<P, A> {
    port: P,
    aftermath: A,
}

#[derive(Debug, Deserialize, Validate)]
struct TransferBody {
    #[garde(skip)]
    user_id: String,
}

async fn transfer_ownership<P, A>(
    State(state): State<TransferState<P, A>>,
    viewer: Option<Extension<UserId>>,
    Path(raw_workspace): Path<String>,
    ValidatedJson(body): ValidatedJson<TransferBody>,
) -> Response
where
    P: WorkspaceOwnershipTransferPort,
    A: WorkspaceMembershipAftermath,
{
    let (owner, _) = match human_principal(viewer, "workspace_ownership_transfer") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let workspace = match workspace(raw_workspace) {
        Ok(value) => value,
        Err(value) => return value,
    };
    let target = match user(body.user_id) {
        Ok(value) => value,
        Err(value) => return value,
    };
    match state
        .port
        .transfer_ownership(&owner, &workspace, &target)
        .await
    {
        Ok(OwnershipTransferOutcome::Transferred {
            previous_owner,
            new_owner,
        }) => {
            announce_viewer(&state.aftermath, &previous_owner).await;
            announce_viewer(&state.aftermath, &new_owner).await;
            Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(OwnershipTransferOutcome::WorkspaceNotFound) => {
            response(StatusCode::NOT_FOUND, "workspace not found", None)
        }
        Ok(OwnershipTransferOutcome::Forbidden) => {
            response(StatusCode::FORBIDDEN, "forbidden", None)
        }
        Ok(OwnershipTransferOutcome::MemberNotFound) => response(
            StatusCode::NOT_FOUND,
            "workspace human member not found",
            None,
        ),
        Err(cause) => failed("workspace ownership transfer failed", &cause),
    }
}

#[allow(clippy::result_large_err)]
fn human_principal(
    viewer: Option<Extension<UserId>>,
    operation: &'static str,
) -> Result<(UserId, Principal), Response> {
    let Some(Extension(viewer)) = viewer else {
        tracing::error!(
            operation,
            "workspace membership route reached without human principal"
        );
        return Err(internal());
    };
    Ok((viewer.clone(), Principal::Human { user_id: viewer }))
}

#[allow(clippy::result_large_err)]
fn workspace(raw: String) -> Result<WorkspaceId, Response> {
    WorkspaceId::parse(raw)
        .map_err(|_| response(StatusCode::NOT_FOUND, "workspace not found", None))
}

#[allow(clippy::result_large_err)]
fn agent(raw: String) -> Result<AgentId, Response> {
    AgentId::parse(raw).map_err(|_| response(StatusCode::NOT_FOUND, "agent not found", None))
}

#[allow(clippy::result_large_err)]
fn user(raw: String) -> Result<UserId, Response> {
    UserId::parse(raw).map_err(|_| {
        response(
            StatusCode::NOT_FOUND,
            "workspace human member not found",
            None,
        )
    })
}

async fn list<P, A>(
    State(state): State<MembershipState<P, A>>,
    viewer: Option<Extension<UserId>>,
    Path(raw): Path<String>,
) -> Response
where
    P: WorkspaceMembershipPort,
    A: WorkspaceMembershipAftermath + MembershipRuntimeStop,
{
    let (_, principal) = match human_principal(viewer, "workspace_members_list") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let workspace = match workspace(raw) {
        Ok(value) => value,
        Err(value) => return value,
    };
    match state
        .port
        .list_workspace_members(&principal, &workspace)
        .await
    {
        Ok(WorkspaceRosterRead::WorkspaceNotFound) => {
            response(StatusCode::NOT_FOUND, "workspace not found", None)
        }
        Ok(WorkspaceRosterRead::Forbidden) => response(StatusCode::FORBIDDEN, "forbidden", None),
        Ok(WorkspaceRosterRead::Visible(roster)) => {
            Json(WorkspaceRosterDto::from(roster)).into_response()
        }
        Err(cause) => failed("workspace member list failed", &cause),
    }
}

#[derive(Debug, Deserialize, Validate)]
struct RoleBody {
    #[garde(skip)]
    role: String,
}

async fn change_role<P, A>(
    State(state): State<MembershipState<P, A>>,
    viewer: Option<Extension<UserId>>,
    Path((raw_workspace, raw_user)): Path<(String, String)>,
    request: Request,
) -> Response
where
    P: WorkspaceMembershipPort + WorkspaceGovernanceFactsRead,
    A: WorkspaceMembershipAftermath,
{
    let (_, principal) = match human_principal(viewer, "workspace_human_role_change") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let workspace = match workspace(raw_workspace) {
        Ok(value) => value,
        Err(value) => return value,
    };
    let user = match user(raw_user) {
        Ok(value) => value,
        Err(value) => return value,
    };
    let facts = match state
        .port
        .read_governance_facts(&principal, &workspace)
        .await
    {
        Ok(value) => value,
        Err(cause) => return failed("workspace role preflight failed", &cause),
    };
    match governance_floor_verdict(&principal, facts, WorkspaceRole::Owner) {
        WorkspaceGovernanceFloor::WorkspaceNotFound => {
            return response(StatusCode::NOT_FOUND, "workspace not found", None);
        }
        WorkspaceGovernanceFloor::Denied => {
            return response(StatusCode::FORBIDDEN, "forbidden", None);
        }
        WorkspaceGovernanceFloor::Governs => {}
    }
    let body = match ValidatedJson::<RoleBody>::from_request(request, &()).await {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(role) = WorkspaceRole::parse(&body.role) else {
        return invalid();
    };
    if role == WorkspaceRole::Owner {
        return response(
            StatusCode::BAD_REQUEST,
            "owner role changes are not supported",
            Some("owner_role_change_unsupported"),
        );
    }
    match state
        .port
        .change_human_role(&principal, &workspace, &user, role)
        .await
    {
        Ok(HumanRoleChangeOutcome::Updated(change)) => {
            if let Some(committed) = change.committed.as_ref() {
                announce_viewer(&state.aftermath, committed).await;
            }
            Json(RoleChangeDto::from(*change)).into_response()
        }
        Ok(HumanRoleChangeOutcome::WorkspaceNotFound) => {
            response(StatusCode::NOT_FOUND, "workspace not found", None)
        }
        Ok(HumanRoleChangeOutcome::Forbidden) => response(StatusCode::FORBIDDEN, "forbidden", None),
        Ok(HumanRoleChangeOutcome::MemberNotFound) => response(
            StatusCode::NOT_FOUND,
            "workspace human member not found",
            None,
        ),
        Ok(HumanRoleChangeOutcome::OwnerRoleChangeUnsupported) => response(
            StatusCode::BAD_REQUEST,
            "owner role changes are not supported",
            Some("owner_role_change_unsupported"),
        ),
        Err(cause) => failed("workspace role change failed", &cause),
    }
}

async fn remove_agent<P, A>(
    State(state): State<MembershipState<P, A>>,
    viewer: Option<Extension<UserId>>,
    Path((raw_workspace, raw_agent)): Path<(String, String)>,
) -> Response
where
    P: WorkspaceMembershipPort,
    A: WorkspaceMembershipAftermath + MembershipRuntimeStop,
{
    let (_, principal) = match human_principal(viewer, "workspace_agent_remove") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let workspace = match WorkspaceId::parse(raw_workspace) {
        Ok(value) => value,
        Err(_) => return StatusCode::NO_CONTENT.into_response(),
    };
    let agent = match AgentId::parse(raw_agent) {
        Ok(value) => value,
        Err(_) => return StatusCode::NO_CONTENT.into_response(),
    };
    let runtime_stops = match state
        .port
        .preflight_remove_workspace_agent(&principal, &workspace, &agent)
        .await
    {
        Ok(WorkspaceAgentRemovalPreflight::Ready { runtime_stops }) => runtime_stops,
        Ok(WorkspaceAgentRemovalPreflight::NoSuchWorkspaceOrMember) => {
            return StatusCode::NO_CONTENT.into_response();
        }
        Ok(WorkspaceAgentRemovalPreflight::Forbidden) => {
            return response(StatusCode::FORBIDDEN, "forbidden", None);
        }
        Ok(WorkspaceAgentRemovalPreflight::AgentNotOwned) => {
            return response(
                StatusCode::FORBIDDEN,
                "you can only remove agents you created",
                Some("agent_not_owned"),
            );
        }
        Err(cause) => return failed("workspace agent removal preflight failed", &cause),
    };
    for conversation in runtime_stops {
        if let Err(cause) = state.aftermath.stop(&conversation, &agent).await {
            tracing::warn!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                conversation_id = conversation.as_str(),
                agent_id = agent.as_str(),
                "workspace member removal runtime stop failed; continuing"
            );
        }
    }
    match state
        .port
        .remove_workspace_agent(&principal, &workspace, &agent)
        .await
    {
        Ok(WorkspaceAgentRemovalOutcome::Removed(committed)) => {
            for notice in &committed.notices {
                if let Err(cause) = state.aftermath.stop(&notice.conversation_id, &agent).await {
                    tracing::warn!(
                        error_chain = crate::foundation::safe_log_field::safe_display(
                            &(error::describe(&cause))
                        ),
                        conversation_id = notice.conversation_id.as_str(),
                        agent_id = agent.as_str(),
                        "workspace member removal authoritative post-commit runtime stop failed; continuing"
                    );
                }
            }
            announce(&state.aftermath, &committed).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(WorkspaceAgentRemovalOutcome::NoSuchWorkspaceOrMember) => {
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(WorkspaceAgentRemovalOutcome::Forbidden) => {
            response(StatusCode::FORBIDDEN, "forbidden", None)
        }
        Ok(WorkspaceAgentRemovalOutcome::AgentNotOwned) => response(
            StatusCode::FORBIDDEN,
            "you can only remove agents you created",
            Some("agent_not_owned"),
        ),
        Err(cause) => failed("workspace agent removal failed", &cause),
    }
}

async fn remove_human<P, A>(
    State(state): State<MembershipState<P, A>>,
    viewer: Option<Extension<UserId>>,
    Path((raw_workspace, raw_user)): Path<(String, String)>,
) -> Response
where
    P: WorkspaceMembershipPort,
    A: WorkspaceMembershipAftermath,
{
    let (_, principal) = match human_principal(viewer, "workspace_human_remove") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let workspace = match workspace(raw_workspace) {
        Ok(value) => value,
        Err(value) => return value,
    };
    let user = match user(raw_user) {
        Ok(value) => value,
        Err(value) => return value,
    };
    match state
        .port
        .remove_workspace_human(&principal, &workspace, &user)
        .await
    {
        Ok(WorkspaceHumanRemovalOutcome::Removed(committed)) => {
            announce(&state.aftermath, &committed).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(WorkspaceHumanRemovalOutcome::WorkspaceNotFound) => {
            response(StatusCode::NOT_FOUND, "workspace not found", None)
        }
        Ok(WorkspaceHumanRemovalOutcome::Forbidden) => {
            response(StatusCode::FORBIDDEN, "forbidden", None)
        }
        Ok(WorkspaceHumanRemovalOutcome::MemberNotFound) => response(
            StatusCode::NOT_FOUND,
            "workspace human member not found",
            None,
        ),
        Ok(WorkspaceHumanRemovalOutcome::OwnerRemovalUnsupported) => response(
            StatusCode::BAD_REQUEST,
            "the workspace owner cannot be removed",
            Some("owner_removal_unsupported"),
        ),
        Err(cause) => failed("workspace human removal failed", &cause),
    }
}

async fn leave<P, A>(
    State(state): State<MembershipState<P, A>>,
    viewer: Option<Extension<UserId>>,
    Path(raw): Path<String>,
) -> Response
where
    P: WorkspaceMembershipPort,
    A: WorkspaceMembershipAftermath,
{
    let (viewer, _) = match human_principal(viewer, "workspace_leave") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let workspace = match workspace(raw) {
        Ok(value) => value,
        Err(value) => return value,
    };
    match state.port.leave_workspace(&viewer, &workspace).await {
        Ok(WorkspaceLeaveOutcome::Left(committed)) => {
            announce(&state.aftermath, &committed).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(WorkspaceLeaveOutcome::WorkspaceNotFound) => {
            response(StatusCode::NOT_FOUND, "workspace not found", None)
        }
        Ok(WorkspaceLeaveOutcome::NotMember) => response(
            StatusCode::FORBIDDEN,
            "not a member of this workspace",
            Some("not_member"),
        ),
        Ok(WorkspaceLeaveOutcome::OwnerRemovalUnsupported) => response(
            StatusCode::BAD_REQUEST,
            "the workspace owner cannot be removed",
            Some("owner_removal_unsupported"),
        ),
        Err(cause) => failed("workspace leave failed", &cause),
    }
}

async fn read_capabilities<P, A>(
    State(state): State<MembershipState<P, A>>,
    viewer: Option<Extension<UserId>>,
    Path((raw_workspace, raw_agent)): Path<(String, String)>,
) -> Response
where
    P: WorkspaceCapabilityPort,
    A: WorkspaceMembershipAftermath,
{
    let (_, principal) = match human_principal(viewer, "workspace_capability_read") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let workspace = match workspace(raw_workspace) {
        Ok(value) => value,
        Err(value) => return value,
    };
    let agent = match agent(raw_agent) {
        Ok(value) => value,
        Err(value) => return value,
    };
    match state
        .port
        .read_capabilities(&principal, &workspace, &agent)
        .await
    {
        Ok(outcome) => capability_read(outcome),
        Err(cause) => failed("workspace capability read failed", &cause),
    }
}

#[derive(Debug, Deserialize, Validate)]
struct CapabilityBody {
    #[garde(skip)]
    capabilities: Vec<String>,
}

async fn replace_capabilities<P, A>(
    State(state): State<MembershipState<P, A>>,
    viewer: Option<Extension<UserId>>,
    Path((raw_workspace, raw_agent)): Path<(String, String)>,
    request: Request,
) -> Response
where
    P: WorkspaceCapabilityPort,
    A: WorkspaceMembershipAftermath,
{
    let (_, principal) = match human_principal(viewer, "workspace_capability_replace") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let workspace = match workspace(raw_workspace) {
        Ok(value) => value,
        Err(value) => return value,
    };
    let agent = match agent(raw_agent) {
        Ok(value) => value,
        Err(value) => return value,
    };
    // Authorization and pair existence precede body admission.  The write repeats
    // both under its transaction, closing the demotion/removal race.
    match state
        .port
        .preflight_replace_capabilities(&principal, &workspace, &agent)
        .await
    {
        Ok(CapabilityReplacePreflight::Authorized) => {}
        Ok(CapabilityReplacePreflight::WorkspaceNotFound) => {
            return response(StatusCode::NOT_FOUND, "workspace not found", None);
        }
        Ok(CapabilityReplacePreflight::Forbidden) => {
            return response(StatusCode::FORBIDDEN, "forbidden", None);
        }
        Ok(CapabilityReplacePreflight::AgentNotMember) => {
            return response(
                StatusCode::NOT_FOUND,
                "agent is not a member of this workspace",
                None,
            );
        }
        Err(cause) => return failed("workspace capability preflight failed", &cause),
    }
    let body = match ValidatedJson::<CapabilityBody>::from_request(request, &()).await {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(capabilities) = body
        .capabilities
        .iter()
        .map(|value| Capability::parse(value))
        .collect::<Option<Vec<_>>>()
    else {
        return invalid();
    };
    match state
        .port
        .replace_capabilities(&principal, &workspace, &agent, &capabilities)
        .await
    {
        Ok(CapabilityReplaceOutcome::Replaced(value)) => {
            Json(CapabilityDto::from(value)).into_response()
        }
        Ok(CapabilityReplaceOutcome::WorkspaceNotFound) => {
            response(StatusCode::NOT_FOUND, "workspace not found", None)
        }
        Ok(CapabilityReplaceOutcome::Forbidden) => {
            response(StatusCode::FORBIDDEN, "forbidden", None)
        }
        Ok(CapabilityReplaceOutcome::AgentNotMember) => response(
            StatusCode::NOT_FOUND,
            "agent is not a member of this workspace",
            None,
        ),
        Err(cause) => failed("workspace capability replacement failed", &cause),
    }
}

fn capability_read(outcome: CapabilityReadOutcome) -> Response {
    match outcome {
        CapabilityReadOutcome::Found(value) => Json(CapabilityDto::from(value)).into_response(),
        CapabilityReadOutcome::WorkspaceNotFound => {
            response(StatusCode::NOT_FOUND, "workspace not found", None)
        }
        CapabilityReadOutcome::Forbidden => response(StatusCode::FORBIDDEN, "forbidden", None),
        CapabilityReadOutcome::AgentNotMember => response(
            StatusCode::NOT_FOUND,
            "agent is not a member of this workspace",
            None,
        ),
    }
}

#[derive(Serialize)]
struct WorkspaceAgentDto {
    id: String,
    name: String,
    display_name: String,
    runtime: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    fast_mode: Option<bool>,
    host_id: String,
    host_label: Option<String>,
    description: Option<String>,
    session_scope: String,
    created_by_user_id: String,
    avatar_url: Option<String>,
    materialization_status: String,
    materialization_failed_code: Option<String>,
    materialization_failed_at: Option<i64>,
    deletion_started_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}
#[derive(Serialize)]
struct WorkspaceHumanDto {
    id: String,
    name: String,
    display_name: String,
    avatar_url: Option<String>,
    created_at: i64,
    updated_at: i64,
    workspace_id: String,
    user_id: String,
    joined_at: i64,
    role: &'static str,
}
#[derive(Serialize)]
struct WorkspaceRosterDto {
    members: Vec<WorkspaceAgentDto>,
    human_members: Vec<WorkspaceHumanDto>,
    viewer_role: Option<&'static str>,
}

impl From<WorkspaceRoster> for WorkspaceRosterDto {
    fn from(value: WorkspaceRoster) -> Self {
        let workspace_id = value.workspace_id.as_str().to_owned();
        Self {
            members: value
                .agents
                .into_iter()
                .map(|row| WorkspaceAgentDto {
                    avatar_url: row
                        .avatar_storage_key
                        .as_ref()
                        .filter(|key| !key.is_empty())
                        .map(|_| {
                            format!(
                                "/api/agents/{}/avatar?v={}",
                                row.id.as_str(),
                                row.updated_at
                            )
                        }),
                    id: row.id.as_str().to_owned(),
                    name: row.name,
                    display_name: row.display_name,
                    runtime: row.runtime,
                    model: row.model,
                    reasoning_effort: row.reasoning_effort,
                    fast_mode: row.fast_mode,
                    host_id: row.host_id,
                    host_label: row
                        .host_display_label
                        .map(|label| label.trim_matches(is_ecmascript_trim_whitespace).to_owned())
                        .filter(|label| !label.is_empty())
                        .or(row.host_name.filter(|name| !name.is_empty())),
                    description: row.description,
                    session_scope: row.session_scope,
                    created_by_user_id: row.created_by_user_id.as_str().to_owned(),
                    materialization_status: row.materialization_status,
                    materialization_failed_code: row.materialization_failed_code,
                    materialization_failed_at: row.materialization_failed_at,
                    deletion_started_at: row.deletion_started_at,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                })
                .collect(),
            human_members: value
                .humans
                .into_iter()
                .map(|row| WorkspaceHumanDto {
                    avatar_url: row
                        .avatar_storage_key
                        .as_ref()
                        .filter(|key| !key.is_empty())
                        .map(|_| {
                            format!("/api/users/{}/avatar?v={}", row.id.as_str(), row.updated_at)
                        }),
                    id: row.id.as_str().to_owned(),
                    name: row.name,
                    display_name: row.display_name,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                    workspace_id: workspace_id.clone(),
                    user_id: row.id.as_str().to_owned(),
                    joined_at: row.joined_at_ms,
                    role: row.role.as_str(),
                })
                .collect(),
            viewer_role: value.viewer_role.map(|role| role.as_str()),
        }
    }
}

#[derive(Serialize)]
struct RoleChangeDto {
    member: WorkspaceUserMemberDto,
    human_member: WorkspaceHumanDto,
}
#[derive(Serialize)]
struct WorkspaceUserMemberDto {
    workspace_id: String,
    user_id: String,
    joined_at: i64,
    role: &'static str,
}
impl From<silverhand_backend_core::identity_access::access::HumanRoleChange> for RoleChangeDto {
    fn from(value: silverhand_backend_core::identity_access::access::HumanRoleChange) -> Self {
        let workspace_id = value.workspace_id.as_str().to_owned();
        let row = value.member;
        let member = WorkspaceUserMemberDto {
            workspace_id: workspace_id.clone(),
            user_id: row.id.as_str().to_owned(),
            joined_at: row.joined_at_ms,
            role: row.role.as_str(),
        };
        let human_member = WorkspaceHumanDto {
            avatar_url: row
                .avatar_storage_key
                .as_ref()
                .filter(|key| !key.is_empty())
                .map(|_| format!("/api/users/{}/avatar?v={}", row.id.as_str(), row.updated_at)),
            id: row.id.as_str().to_owned(),
            name: row.name,
            display_name: row.display_name,
            created_at: row.created_at,
            updated_at: row.updated_at,
            workspace_id,
            user_id: row.id.as_str().to_owned(),
            joined_at: row.joined_at_ms,
            role: row.role.as_str(),
        };
        Self {
            member,
            human_member,
        }
    }
}

#[derive(Serialize)]
struct CapabilityDto {
    workspace_id: String,
    agent_id: String,
    capabilities: Vec<&'static str>,
}
impl From<AgentCapabilitySet> for CapabilityDto {
    fn from(value: AgentCapabilitySet) -> Self {
        Self {
            workspace_id: value.workspace_id.as_str().to_owned(),
            agent_id: value.agent_id.as_str().to_owned(),
            capabilities: value.capabilities.iter().map(Capability::as_str).collect(),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
}
fn response(status: StatusCode, message: &'static str, code: Option<&'static str>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message,
            code,
        }),
    )
        .into_response()
}
fn invalid() -> Response {
    response(StatusCode::UNPROCESSABLE_ENTITY, "invalid input", None)
}
fn failed(message: &'static str, cause: &(impl std::error::Error + 'static)) -> Response {
    tracing::error!(
        error_chain = crate::foundation::safe_log_field::safe_display(&(error::describe(cause))),
        message
    );
    internal()
}

async fn announce<A: WorkspaceMembershipAftermath>(
    aftermath: &A,
    changed: &WorkspaceMembershipChanged,
) {
    if let Err(cause) = aftermath.announce(changed).await {
        tracing::warn!(
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
            workspace_id = changed.workspace_id.as_str(),
            "workspace membership committed but post-commit announcement failed"
        );
    }
}

async fn announce_viewer<A: WorkspaceMembershipAftermath>(
    aftermath: &A,
    changed: &WorkspaceViewerMembershipChanged,
) {
    if let Err(cause) = aftermath.announce_viewer(changed).await {
        tracing::warn!(
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
            workspace_id = changed.workspace_id.as_str(),
            user_id = changed.user_id.as_str(),
            "workspace role committed but viewer announcement failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, header::CONTENT_TYPE},
    };
    use silverhand_backend_core::foundation::{
        identity::ConversationId, persistence::PersistenceFailure,
    };
    use silverhand_backend_sqlite::identity_access::access::SqliteAccessStore;
    use sqlx::{Connection, Executor};
    use tower::ServiceExt;

    #[tokio::test]
    async fn admitted_membership_requests_reject_invalid_bodies_but_allow_empty_capabilities() {
        for body in [r#"{}"#, r#"{"role":null}"#, r#"["admin"]"#] {
            let response = route_call(
                Method::PATCH,
                "/api/workspaces/workspace/members/users/target/role",
                "owner",
                Some(body),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body}"
            );
        }
        let response = route_call(
            Method::PUT,
            "/api/workspaces/workspace/agents/agent/capabilities",
            "owner",
            Some(r#"{"capabilities":["be_root"]}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let response = route_call(
            Method::PUT,
            "/api/workspaces/workspace/agents/agent/capabilities",
            "owner",
            Some(r#"{"capabilities":[]}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["capabilities"], serde_json::json!([]));
    }

    #[derive(Clone, Default)]
    struct Effects {
        announcements: Arc<Mutex<Vec<WorkspaceMembershipChanged>>>,
        viewer_announcements: Arc<Mutex<Vec<WorkspaceViewerMembershipChanged>>>,
        stops: Arc<Mutex<Vec<(ConversationId, AgentId)>>>,
    }
    impl WorkspaceMembershipAftermath for Effects {
        type Error = Infallible;
        async fn announce(&self, changed: &WorkspaceMembershipChanged) -> Result<(), Self::Error> {
            self.announcements.lock().unwrap().push(changed.clone());
            Ok(())
        }
        async fn announce_viewer(
            &self,
            changed: &WorkspaceViewerMembershipChanged,
        ) -> Result<(), Self::Error> {
            self.viewer_announcements
                .lock()
                .unwrap()
                .push(changed.clone());
            Ok(())
        }
    }
    impl MembershipRuntimeStop for Effects {
        type Error = Infallible;
        async fn stop(
            &self,
            conversation: &ConversationId,
            agent: &AgentId,
        ) -> Result<(), Self::Error> {
            self.stops
                .lock()
                .unwrap()
                .push((conversation.clone(), agent.clone()));
            Ok(())
        }
    }

    async fn route_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace-membership-routes.sqlite");
        let mut db = crate::current_schema::create_current_schema(&path).await;
        db.execute("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('owner','owner','Owner',1,1),('member','member','Member',1,1),('target','target','Target',1,1)").await.unwrap();
        db.execute("INSERT INTO hosts(id,name,kind,created_at) VALUES('host','host','local',1)")
            .await
            .unwrap();
        db.execute("INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id,display_name,session_scope) VALUES('agent','agent','codex','host',1,1,'owner','Agent','conversation')").await.unwrap();
        db.execute("INSERT INTO workspaces(id,name,name_key,created_at,updated_at,owner_user_id) VALUES('workspace','Workspace','workspace',1,1,'owner')").await.unwrap();
        db.execute("INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('workspace','owner',1,'owner'),('workspace','member',2,'member'),('workspace','target',3,'member')").await.unwrap();
        db.execute("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('workspace','agent',1)").await.unwrap();
        db.execute("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES('workspace','agent','send_messages'),('workspace','agent','read_messages')").await.unwrap();
        db.execute("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at,visibility) VALUES('channel','workspace','workspace','open','Channel','channel','idle',1,1,'public')").await.unwrap();
        db.execute("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('channel','owner',1),('channel','member',2),('channel','target',3)").await.unwrap();
        db.execute("INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('channel','agent',1)").await.unwrap();
        db.close().await.unwrap();
        (dir, path)
    }

    async fn route_call(method: Method, path: &str, viewer: &str, body: Option<&str>) -> Response {
        let (_dir, database) = route_fixture().await;
        let store = SqliteAccessStore::open(&database).await.unwrap();
        let app =
            router(store, Effects::default()).layer(Extension(UserId::parse(viewer).unwrap()));
        let mut request = Request::builder().method(method).uri(path);
        let payload = body.unwrap_or_default();
        if body.is_some() {
            request = request.header(CONTENT_TYPE, "application/json");
        }
        app.oneshot(request.body(Body::from(payload.to_owned())).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn all_seven_workspace_membership_and_capability_methods_reach_real_store_success_contracts()
     {
        let cases = [
            (
                Method::GET,
                "/api/workspaces/workspace/members",
                None,
                StatusCode::OK,
            ),
            (
                Method::PATCH,
                "/api/workspaces/workspace/members/users/target/role",
                Some(r#"{"role":"admin"}"#),
                StatusCode::OK,
            ),
            (
                Method::DELETE,
                "/api/workspaces/workspace/members/agent",
                None,
                StatusCode::NO_CONTENT,
            ),
            (
                Method::DELETE,
                "/api/workspaces/workspace/members/users/target",
                None,
                StatusCode::NO_CONTENT,
            ),
            (
                Method::POST,
                "/api/workspaces/workspace/leave",
                None,
                StatusCode::NO_CONTENT,
            ),
            (
                Method::GET,
                "/api/workspaces/workspace/agents/agent/capabilities",
                None,
                StatusCode::OK,
            ),
            (
                Method::PUT,
                "/api/workspaces/workspace/agents/agent/capabilities",
                Some(r#"{"capabilities":["send_messages","read_messages","send_messages"]}"#),
                StatusCode::OK,
            ),
        ];
        for (method, path, body, status) in cases {
            let viewer = if path.ends_with("/leave") {
                "member"
            } else {
                "owner"
            };
            let response = route_call(method.clone(), path, viewer, body).await;
            assert_eq!(response.status(), status, "{method} {path}");
            if method == Method::PUT {
                let json: serde_json::Value = serde_json::from_slice(
                    &to_bytes(response.into_body(), usize::MAX).await.unwrap(),
                )
                .unwrap();
                assert_eq!(
                    json["capabilities"],
                    serde_json::json!(["read_messages", "send_messages"])
                );
            }
        }
    }

    #[tokio::test]
    async fn all_seven_workspace_methods_pin_their_primary_missing_container_refusal() {
        let cases = [
            (
                Method::GET,
                "/api/workspaces/missing/members",
                None,
                StatusCode::NOT_FOUND,
            ),
            (
                Method::PATCH,
                "/api/workspaces/missing/members/users/target/role",
                Some(r#"{"role":"admin"}"#),
                StatusCode::NOT_FOUND,
            ),
            (
                Method::DELETE,
                "/api/workspaces/missing/members/agent",
                None,
                StatusCode::NO_CONTENT,
            ),
            (
                Method::DELETE,
                "/api/workspaces/missing/members/users/target",
                None,
                StatusCode::NOT_FOUND,
            ),
            (
                Method::POST,
                "/api/workspaces/missing/leave",
                None,
                StatusCode::NOT_FOUND,
            ),
            (
                Method::GET,
                "/api/workspaces/missing/agents/agent/capabilities",
                None,
                StatusCode::NOT_FOUND,
            ),
            (
                Method::PUT,
                "/api/workspaces/missing/agents/agent/capabilities",
                Some(r#"{"capabilities":[]}"#),
                StatusCode::NOT_FOUND,
            ),
        ];
        for (method, path, body, status) in cases {
            let response = route_call(method.clone(), path, "owner", body).await;
            assert_eq!(response.status(), status, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn capability_put_authorizes_and_resolves_membership_before_body_shape() {
        let forbidden = route_call(
            Method::PUT,
            "/api/workspaces/workspace/agents/agent/capabilities",
            "member",
            Some("not-json"),
        )
        .await;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        let missing = route_call(
            Method::PUT,
            "/api/workspaces/workspace/agents/missing/capabilities",
            "owner",
            Some("not-json"),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[derive(Clone)]
    struct RemovalPort {
        outcome: WorkspaceAgentRemovalOutcome,
    }
    impl WorkspaceMembershipPort for RemovalPort {
        async fn list_workspace_members(
            &self,
            _: &Principal,
            _: &WorkspaceId,
        ) -> Result<WorkspaceRosterRead, PersistenceFailure> {
            unreachable!()
        }
        async fn change_human_role(
            &self,
            _: &Principal,
            _: &WorkspaceId,
            _: &UserId,
            _: WorkspaceRole,
        ) -> Result<HumanRoleChangeOutcome, PersistenceFailure> {
            unreachable!()
        }
        async fn remove_workspace_agent(
            &self,
            _: &Principal,
            _: &WorkspaceId,
            _: &AgentId,
        ) -> Result<WorkspaceAgentRemovalOutcome, PersistenceFailure> {
            Ok(self.outcome.clone())
        }
        async fn preflight_remove_workspace_agent(
            &self,
            _: &Principal,
            _: &WorkspaceId,
            _: &AgentId,
        ) -> Result<WorkspaceAgentRemovalPreflight, PersistenceFailure> {
            Ok(WorkspaceAgentRemovalPreflight::Ready {
                runtime_stops: vec![ConversationId::parse("hint").unwrap()],
            })
        }
        async fn remove_workspace_human(
            &self,
            _: &Principal,
            _: &WorkspaceId,
            _: &UserId,
        ) -> Result<WorkspaceHumanRemovalOutcome, PersistenceFailure> {
            unreachable!()
        }
        async fn leave_workspace(
            &self,
            _: &UserId,
            _: &WorkspaceId,
        ) -> Result<WorkspaceLeaveOutcome, PersistenceFailure> {
            unreachable!()
        }
    }

    fn committed(conversation: &str) -> WorkspaceMembershipChanged {
        WorkspaceMembershipChanged {
            workspace_id: WorkspaceId::parse("workspace").unwrap(),
            notices: vec![silverhand_backend_core::conversation_messaging::messaging::collaboration::membership::CommittedMembershipNotice {
                conversation_id: ConversationId::parse(conversation).unwrap(),
                workspace_id: WorkspaceId::parse("workspace").unwrap(),
                message_id: silverhand_backend_core::conversation_messaging::messaging::MessageId::parse("notice").unwrap(),
                content: "membership changed".to_owned(),
                metadata: "{}".to_owned(),
                seq: 1,
                created_at: 1,
            }],
            viewer: None,
        }
    }

    #[tokio::test]
    async fn workspace_agent_remove_stops_preflight_hint_and_post_commit_authoritative_set_only_after_commit()
     {
        let effects = Effects::default();
        let response = remove_agent(
            State(MembershipState {
                port: RemovalPort {
                    outcome: WorkspaceAgentRemovalOutcome::Removed(committed("authoritative")),
                },
                aftermath: effects.clone(),
            }),
            Some(Extension(UserId::parse("owner").unwrap())),
            Path(("workspace".to_owned(), "agent".to_owned())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            effects
                .stops
                .lock()
                .unwrap()
                .iter()
                .map(|(conversation, _)| conversation.as_str())
                .collect::<Vec<_>>(),
            vec!["hint", "authoritative"]
        );
        assert_eq!(effects.announcements.lock().unwrap().len(), 1);

        let refused_effects = Effects::default();
        let response = remove_agent(
            State(MembershipState {
                port: RemovalPort {
                    outcome: WorkspaceAgentRemovalOutcome::Forbidden,
                },
                aftermath: refused_effects.clone(),
            }),
            Some(Extension(UserId::parse("owner").unwrap())),
            Path(("workspace".to_owned(), "agent".to_owned())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            refused_effects
                .stops
                .lock()
                .unwrap()
                .iter()
                .map(|(conversation, _)| conversation.as_str())
                .collect::<Vec<_>>(),
            vec!["hint"]
        );
        assert!(refused_effects.announcements.lock().unwrap().is_empty());
    }
}
