//! Authenticated cold faces for the agent-pane workspace filesystem:
//! `GET /api/agents/{id}/workspace/tree` and `GET /api/agents/{id}/workspace/file`.
//!
//! Both run the SAME admission ladder before any daemon round trip, in the order
//! the product contract fixes:
//!
//!  1. PRINCIPAL — the human proves the authenticating perimeter ran; its absence
//!     is a composition fault, a fail-closed `500` reading nothing.
//!  2. VISIBILITY ([`can_view_agent`]) — an absent row, a caller outside the ACL,
//!     and a hidden stage all collapse to ONE `404 {"error":"agent not found"}`, so
//!     the surface is never an existence oracle. The resolved row is bound to the
//!     requested id BEFORE the gate; a bundle for another agent is a safe `500`.
//!  3. AUTHORITY ([`can_view_agent_files`], the owner-or-self alias of
//!     [`can_edit_agent`]) — a co-workspace member who cleared visibility but does
//!     not own the agent is refused `403 {"error":"forbidden","code":"forbidden"}`;
//!     an agent's files are its private cognition.
//!  4. SCOPE — `?workspace_id=` classifies into the broad home view (absent), an
//!     empty-string `400 invalid_workspace_id`, or a named workspace resolved on ONE
//!     transaction and judged five ways by [`resolve_agent_workspace_scope`].
//!  5. (file only) PATH — `?path=` must be present and nonempty; the string is
//!     OPAQUE and crosses verbatim, as the daemon is the sole virtual-path authority.
//!  6. DISPATCH — the agent's host (off the row already read) drives the data-plane
//!     command; any fault converges to the closed product taxonomy and its status.
//!
//! Product-authored bytes only: the daemon's fault strings never cross the typed
//! port, so a data-plane fault renders `{error,code}` from this crate's own message
//! and the code's HTTP status.

use axum::{
    Json, Router,
    extract::{Extension, Path, RawQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use serde_json::json;
use silverhand_backend_core::{
    foundation::identity::{AgentId, HostId, Principal, UserId, WorkspaceId},
    identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailReadModel},
    identity_access::capability::{
        agent_authority::{AgentAuthority, can_view_agent_files},
        agent_view::{VisibleStages, can_view_agent},
    },
    workspace_data_plane::{
        AgentWorkspaceScopeRead, AgentWorkspaceScopeRefusal, AgentWorkspaceScopeResolution,
        WorkspaceDataPlane, WorkspaceFileRead, WorkspaceFsFault, WorkspaceFsFaultCode,
        WorkspaceNodeKind, WorkspaceTree, resolve_agent_workspace_scope,
    },
};

use crate::foundation::error;

/// The product route templates, in axum's `{id}` capture syntax.
pub const AGENT_WORKSPACE_TREE_PATH: &str = "/api/agents/{id}/workspace/tree";
pub const AGENT_WORKSPACE_FILE_PATH: &str = "/api/agents/{id}/workspace/file";

const AGENT_NOT_FOUND: &str = "agent not found";
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

#[derive(Clone)]
struct WorkspaceState<R, S, D> {
    agent_reads: R,
    scope_reads: S,
    data_plane: D,
}

pub fn router<R, S, D>(agent_reads: R, scope_reads: S, data_plane: D) -> Router
where
    R: AgentDetailReadModel + Clone + Send + Sync + 'static,
    S: AgentWorkspaceScopeRead + Clone + Send + Sync + 'static,
    D: WorkspaceDataPlane + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(AGENT_WORKSPACE_TREE_PATH, get(tree::<R, S, D>))
        .route(AGENT_WORKSPACE_FILE_PATH, get(file::<R, S, D>))
        .with_state(WorkspaceState {
            agent_reads,
            scope_reads,
            data_plane,
        })
}

/// The first occurrence of a single-value query key, percent- and `+`-decoded by
/// `form_urlencoded`. The raw query rides UNPARSED into the ladder; this decode runs
/// only after the authority gates — for the scope token inside [`authorize`], for the
/// file `path` in the handler once `authorize` has returned. It replaces an eager
/// `Query<T>` extractor, whose parse would emit a native `400` before visibility runs
/// and whose duplicate-key rejection the frontend's idempotent re-sends do not
/// warrant. A repeated key takes the FIRST value; `None` is the key's absence and
/// `Some("")` a present-but-empty value the caller classifies (an empty
/// `workspace_id` is the scope `400`, an empty `path` the forbidden-path `400`).
fn first_query_value(raw: Option<&str>, key: &str) -> Option<String> {
    let raw = raw?;
    form_urlencoded::parse(raw.as_bytes())
        .find(|(candidate, _)| candidate.as_ref() == key)
        .map(|(_, value)| value.into_owned())
}

/// The coordinates the shared ladder produces once a caller is fully admitted: the
/// agent, the host that owns its files, and the resolved scope (`None` is the broad
/// home view).
struct Authorized {
    agent: AgentId,
    host: HostId,
    workspace: Option<WorkspaceId>,
}

async fn tree<R, S, D>(
    Path(agent_id): Path<String>,
    RawQuery(raw): RawQuery,
    viewer: Option<Extension<UserId>>,
    State(state): State<WorkspaceState<R, S, D>>,
) -> Response
where
    R: AgentDetailReadModel,
    S: AgentWorkspaceScopeRead,
    D: WorkspaceDataPlane,
{
    let authorized = match authorize(
        &state.agent_reads,
        &state.scope_reads,
        viewer,
        agent_id,
        raw.as_deref(),
    )
    .await
    {
        Ok(authorized) => authorized,
        Err(response) => return *response,
    };

    match state
        .data_plane
        .read_tree(
            &authorized.agent,
            &authorized.host,
            authorized.workspace.as_ref(),
        )
        .await
    {
        Ok(tree) => Json(TreeResponse {
            tree: tree_body(tree),
        })
        .into_response(),
        Err(fault) => fault_response(fault),
    }
}

async fn file<R, S, D>(
    Path(agent_id): Path<String>,
    RawQuery(raw): RawQuery,
    viewer: Option<Extension<UserId>>,
    State(state): State<WorkspaceState<R, S, D>>,
) -> Response
where
    R: AgentDetailReadModel,
    S: AgentWorkspaceScopeRead,
    D: WorkspaceDataPlane,
{
    let authorized = match authorize(
        &state.agent_reads,
        &state.scope_reads,
        viewer,
        agent_id,
        raw.as_deref(),
    )
    .await
    {
        Ok(authorized) => authorized,
        Err(response) => return *response,
    };

    // The path is classified AFTER scope, matching the product order: an empty
    // `workspace_id` alongside a missing `path` is the scope `400`, not this one.
    // Absent and empty both fail; the daemon owns every other verdict on the string.
    let path = match first_query_value(raw.as_deref(), "path") {
        Some(path) if !path.is_empty() => path,
        _ => {
            return envelope(
                StatusCode::BAD_REQUEST,
                "forbidden_path",
                "path query param is required",
            );
        }
    };

    match state
        .data_plane
        .read_file(
            &authorized.agent,
            &authorized.host,
            authorized.workspace.as_ref(),
            &path,
        )
        .await
    {
        Ok(file) => Json(FileResponse {
            file: file_body(file),
        })
        .into_response(),
        Err(fault) => fault_response(fault),
    }
}

/// The ladder shared by both routes: principal → visibility → authority → scope →
/// host. Returns the admitted coordinates, or the exact refusal `Response` to send.
///
/// The raw query string is carried in UNPARSED and only decoded HERE, after the
/// authority gates — an unauthenticated or unauthorized caller never triggers the
/// `form_urlencoded` walk/allocation over the whole query, and the scope token can
/// never be read before visibility + files clear.
async fn authorize<R, S>(
    agent_reads: &R,
    scope_reads: &S,
    viewer: Option<Extension<UserId>>,
    agent_id: String,
    raw_query: Option<&str>,
) -> Result<Authorized, Box<Response>>
where
    R: AgentDetailReadModel,
    S: AgentWorkspaceScopeRead,
{
    let Some(Extension(viewer)) = viewer else {
        tracing::error!("agent-workspace route reached without its human principal fact");
        return Err(Box::new(internal_error()));
    };
    let principal = Principal::human(viewer.clone());

    // A malformed id names no agent — the same 404 an absent row gets.
    let Ok(agent) = AgentId::parse(agent_id) else {
        return Err(Box::new(not_found()));
    };

    let detail = match agent_reads.read_agent_detail(&agent).await {
        Ok(detail) => detail,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-workspace detail read failed"
            );
            return Err(Box::new(internal_error()));
        }
    };

    // Bind the resolved row to the request before the gate: a bundle whose decoded
    // id is not the one asked for is a resolver defect, the lane's safe 500.
    if let Some(detail) = detail.as_ref()
        && detail.source() != &agent
    {
        tracing::error!(
            requested = agent.as_str(),
            resolved = detail.source().as_str(),
            "agent-workspace detail resolved for the wrong agent"
        );
        return Err(Box::new(internal_error()));
    }

    // Access precedes product: absence, non-membership, and a hidden stage all
    // collapse to one anti-enumeration 404. These routes carry no stage widener.
    let decision = can_view_agent(
        &principal,
        &agent,
        detail.as_ref().map(AgentDetailFacts::visibility),
        &VisibleStages::active_only(),
    );
    let Some(detail) = detail.filter(|_| decision.is_allowed()) else {
        return Err(Box::new(not_found()));
    };

    // Authority is the second, narrower door: visibility can admit a co-workspace
    // member, but an agent's files are owner-or-self only. The creator carried by
    // the bundle IS the authority fact — one value, no second read.
    let authority = AgentAuthority {
        created_by_user_id: Some(detail.created_by_user_id().clone()),
    };
    if !can_view_agent_files(&principal, &agent, Some(&authority)).is_allowed() {
        return Err(Box::new(envelope(
            StatusCode::FORBIDDEN,
            "forbidden",
            "forbidden",
        )));
    }

    // Only now, PAST the authority gates, is the query decoded: the scope token is
    // read here so an unauthorized caller never triggers the query walk, and it can
    // never be classified before visibility + files clear.
    let workspace = match first_query_value(raw_query, "workspace_id") {
        // No token is the broad home view — already authorized by the two gates.
        None => None,
        Some(raw) => {
            // An empty `workspace_id` is the sole classification failure; every
            // nonempty id is a lookup, so `parse` failing IS the empty case.
            let Ok(requested) = WorkspaceId::parse(raw) else {
                return Err(Box::new(envelope(
                    StatusCode::BAD_REQUEST,
                    "invalid_workspace_id",
                    "workspace_id query param must not be empty",
                )));
            };
            let facts = match scope_reads
                .read_agent_workspace_scope(&requested, &viewer, &agent)
                .await
            {
                Ok(facts) => facts,
                Err(cause) => {
                    tracing::error!(
                        error_chain = crate::foundation::safe_log_field::safe_display(
                            &(error::describe(&cause))
                        ),
                        "agent-workspace scope read failed"
                    );
                    return Err(Box::new(internal_error()));
                }
            };
            // The reader stamps the FULL triple it read for — workspace, viewer,
            // subject agent. A drift on any of the three means the bundle answered
            // for different coordinates than the request: a resolver defect, never a
            // product answer, so it fails closed (a safe 500) before any dispatch.
            // Without this a bundle read for (viewer B, agent B) could authorize a
            // (viewer A, agent A) request off B's edges.
            if facts.requested() != &requested
                || facts.viewer() != &viewer
                || facts.subject_agent() != &agent
            {
                tracing::error!(
                    requested = requested.as_str(),
                    resolved = facts.requested().as_str(),
                    "agent-workspace scope resolved for the wrong coordinates"
                );
                return Err(Box::new(internal_error()));
            }
            match resolve_agent_workspace_scope(&facts) {
                AgentWorkspaceScopeResolution::Authorized => Some(requested),
                AgentWorkspaceScopeResolution::Refused(refusal) => {
                    return Err(Box::new(scope_refusal(refusal)));
                }
            }
        }
    };

    // The host coordinate rides the ONE agent row already read, so no second
    // placement lookup can disagree with the visibility snapshot. A stored row
    // always carries a nonempty host id; an empty one is corruption, a safe 500.
    let Ok(host) = HostId::parse(detail.projection().host_id.clone()) else {
        tracing::error!(
            agent = agent.as_str(),
            "agent-workspace agent row carries an empty host id"
        );
        return Err(Box::new(internal_error()));
    };

    Ok(Authorized {
        agent,
        host,
        workspace,
    })
}

/// Map a scope refusal to its product envelope. `forbidden` mirrors the authority
/// door's status and code; the two `404` arms stay distinct codes because the
/// renderer distinguishes "no such workspace" from "the agent is not in it".
fn scope_refusal(refusal: AgentWorkspaceScopeRefusal) -> Response {
    match refusal {
        AgentWorkspaceScopeRefusal::WorkspaceNotFound => envelope(
            StatusCode::NOT_FOUND,
            "workspace_not_found",
            "workspace not found",
        ),
        AgentWorkspaceScopeRefusal::ViewerForbidden => {
            envelope(StatusCode::FORBIDDEN, "forbidden", "forbidden")
        }
        AgentWorkspaceScopeRefusal::AgentNotAMember => envelope(
            StatusCode::NOT_FOUND,
            "workspace_agent_not_member",
            "agent is not a member of this workspace",
        ),
    }
}

/// Map a closed data-plane fault to its HTTP status and stable wire code. The
/// message is the adapter's product-authored one, never a daemon string.
fn fault_response(fault: WorkspaceFsFault) -> Response {
    let (status, code) = match fault.code {
        WorkspaceFsFaultCode::HostAttachPending => {
            (StatusCode::SERVICE_UNAVAILABLE, "host_attach_pending")
        }
        WorkspaceFsFaultCode::DaemonTimeout => (StatusCode::GATEWAY_TIMEOUT, "daemon_timeout"),
        WorkspaceFsFaultCode::DaemonInternalError => {
            (StatusCode::BAD_GATEWAY, "daemon_internal_error")
        }
        WorkspaceFsFaultCode::FsUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "fs_unavailable"),
        WorkspaceFsFaultCode::RegistryUnavailable => {
            (StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable")
        }
        WorkspaceFsFaultCode::ForbiddenPath => (StatusCode::BAD_REQUEST, "forbidden_path"),
        WorkspaceFsFaultCode::NotFound => (StatusCode::NOT_FOUND, "not_found"),
    };
    envelope(status, code, &fault.message)
}

/// The shared `{error,code}` refusal body at a status. Kept as one builder so every
/// non-404 refusal on these routes carries the same shape the frontend switches on.
fn envelope(status: StatusCode, code: &'static str, message: &str) -> Response {
    (status, Json(json!({ "error": message, "code": code }))).into_response()
}

/// The anti-enumeration `404`. It carries NO `code`, byte-matching the shared
/// `{"error":"agent not found"}` every agent surface returns.
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": AGENT_NOT_FOUND })),
    )
        .into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

/// Success body `{ "tree": { "nodes": [...] } }`. The projection is the transport's
/// own: the domain kind enum renders as the wire's `file`/`directory` string, and
/// Byte sizes remain safe integers; mtimes preserve fractional milliseconds.
#[derive(Serialize)]
struct TreeResponse {
    tree: TreeBody,
}

#[derive(Serialize)]
struct TreeBody {
    nodes: Vec<NodeDto>,
}

#[derive(Serialize)]
struct NodeDto {
    path: String,
    kind: &'static str,
    size: i64,
    mtime: f64,
}

/// Success body `{ "file": {...} }`.
#[derive(Serialize)]
struct FileResponse {
    file: FileBody,
}

#[derive(Serialize)]
struct FileBody {
    path: String,
    content: String,
    size: i64,
    mtime: f64,
    truncated: bool,
}

fn tree_body(tree: WorkspaceTree) -> TreeBody {
    TreeBody {
        nodes: tree
            .nodes
            .into_iter()
            .map(|node| NodeDto {
                path: node.path,
                kind: node_kind(node.kind),
                size: node.size.get(),
                mtime: node.mtime.get(),
            })
            .collect(),
    }
}

fn file_body(file: WorkspaceFileRead) -> FileBody {
    FileBody {
        path: file.path,
        content: file.content,
        size: file.size.get(),
        mtime: file.mtime.get(),
        truncated: file.truncated,
    }
}

fn node_kind(kind: WorkspaceNodeKind) -> &'static str {
    match kind {
        WorkspaceNodeKind::File => "file",
        WorkspaceNodeKind::Directory => "directory",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering::SeqCst},
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        foundation::identity::{AgentId, HostId, UserId, WorkspaceId},
        foundation::persistence::PersistenceFailure,
        foundation::wire_number::JsSafeInteger,
        identity_access::agent_detail_read::{
            AgentDetailFacts, AgentDetailProjection, AgentDetailReadModel,
        },
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::runtime::RuntimeKind,
        workspace_data_plane::{
            AgentWorkspaceScopeFacts, AgentWorkspaceScopeRead, WorkspaceDataPlane,
            WorkspaceFileRead, WorkspaceFsFault, WorkspaceFsFaultCode, WorkspaceNodeKind,
            WorkspaceTree, WorkspaceTreeNode,
        },
    };
    use tower::ServiceExt;

    use super::router;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    const VIEWER: &str = "viewer-1";
    const OWNER: &str = "viewer-1";
    const HOST: &str = "host-1";

    fn projection(host_id: &str) -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_string(),
            display_name: "Agent One".to_string(),
            runtime: RuntimeKind::Claude,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            host_id: host_id.to_string(),
            host_display_label: None,
            host_name: None,
            description: None,
            session_scope: "agent".to_string(),
            avatar_storage_key: None,
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1,
            updated_at: 2,
        }
    }

    /// A bundle for `source`, created by `creator`, active stage, on `host`.
    fn facts(source: &str, creator: &str, host: &str) -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent(source),
            user(creator),
            [],
            // A workspace co-member who is NOT the creator: clears visibility, must
            // still fail the owner-only files authority.
            [user("co-member")],
            MaterializationStatus::Active,
            projection(host),
        )
    }

    #[derive(Clone)]
    enum DetailOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
    }

    #[derive(Clone)]
    struct FakeDetail {
        outcome: DetailOutcome,
    }

    impl AgentDetailReadModel for FakeDetail {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            match &self.outcome {
                DetailOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                DetailOutcome::Absent => Ok(None),
                DetailOutcome::Fails => Err(PersistenceFailure::from_source(
                    std::io::Error::other("detail gone"),
                )),
            }
        }
    }

    /// A scope reader that returns pre-seeded facts for the id it is asked about.
    /// By default it stamps the exact triple it was asked for (workspace, viewer,
    /// agent) so the route's source-bind passes; overrides forge a different coordinate.
    #[derive(Clone)]
    struct FakeScope {
        present: bool,
        owner_agent_id: Option<AgentId>,
        human_is_member: bool,
        agent_is_member: bool,
        fails: bool,
        stamp_workspace: Option<WorkspaceId>,
        stamp_viewer: Option<UserId>,
        stamp_agent: Option<AgentId>,
    }

    impl FakeScope {
        fn authorizing() -> Self {
            Self {
                present: true,
                owner_agent_id: None,
                human_is_member: true,
                agent_is_member: true,
                fails: false,
                stamp_workspace: None,
                stamp_viewer: None,
                stamp_agent: None,
            }
        }
    }

    impl AgentWorkspaceScopeRead for FakeScope {
        async fn read_agent_workspace_scope(
            &self,
            workspace: &WorkspaceId,
            viewer: &UserId,
            agent: &AgentId,
        ) -> Result<AgentWorkspaceScopeFacts, PersistenceFailure> {
            if self.fails {
                return Err(PersistenceFailure::from_source(std::io::Error::other(
                    "scope gone",
                )));
            }
            Ok(AgentWorkspaceScopeFacts::new(
                self.stamp_workspace
                    .clone()
                    .unwrap_or_else(|| workspace.clone()),
                self.stamp_viewer.clone().unwrap_or_else(|| viewer.clone()),
                self.stamp_agent.clone().unwrap_or_else(|| agent.clone()),
                self.present,
                self.owner_agent_id.clone(),
                self.human_is_member,
                self.agent_is_member,
            ))
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct Dispatched {
        agent: String,
        host: String,
        workspace: Option<String>,
        path: Option<String>,
    }

    /// Records the exact dispatch it received and returns a canned result or fault,
    /// so a test can prove which command the route drove with which coordinates.
    #[derive(Clone)]
    struct FakeDataPlane {
        result: Result<Answer, WorkspaceFsFaultCode>,
        seen: Arc<Mutex<Vec<Dispatched>>>,
        calls: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    enum Answer {
        Tree(WorkspaceTree),
        File(WorkspaceFileRead),
    }

    impl FakeDataPlane {
        fn ok(answer: Answer) -> Self {
            Self {
                result: Ok(answer),
                seen: Arc::new(Mutex::new(Vec::new())),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn faulting(code: WorkspaceFsFaultCode) -> Self {
            Self {
                result: Err(code),
                seen: Arc::new(Mutex::new(Vec::new())),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn record(&self, dispatched: Dispatched) {
            self.calls.fetch_add(1, SeqCst);
            self.seen.lock().unwrap().push(dispatched);
        }
    }

    fn fault(code: WorkspaceFsFaultCode) -> WorkspaceFsFault {
        WorkspaceFsFault {
            code,
            message: "product message".to_string(),
        }
    }

    impl WorkspaceDataPlane for FakeDataPlane {
        async fn read_tree(
            &self,
            agent: &AgentId,
            host: &HostId,
            workspace: Option<&WorkspaceId>,
        ) -> Result<WorkspaceTree, WorkspaceFsFault> {
            self.record(Dispatched {
                agent: agent.as_str().to_string(),
                host: host.as_str().to_string(),
                workspace: workspace.map(|w| w.as_str().to_string()),
                path: None,
            });
            match &self.result {
                Ok(Answer::Tree(tree)) => Ok(tree.clone()),
                Ok(Answer::File(_)) => panic!("file answer wired into a tree read"),
                Err(code) => Err(fault(*code)),
            }
        }

        async fn read_file(
            &self,
            agent: &AgentId,
            host: &HostId,
            workspace: Option<&WorkspaceId>,
            path: &str,
        ) -> Result<WorkspaceFileRead, WorkspaceFsFault> {
            self.record(Dispatched {
                agent: agent.as_str().to_string(),
                host: host.as_str().to_string(),
                workspace: workspace.map(|w| w.as_str().to_string()),
                path: Some(path.to_string()),
            });
            match &self.result {
                Ok(Answer::File(file)) => Ok(file.clone()),
                Ok(Answer::Tree(_)) => panic!("tree answer wired into a file read"),
                Err(code) => Err(fault(*code)),
            }
        }
    }

    struct Reply {
        status: StatusCode,
        body: Value,
        dispatch_calls: usize,
        last_dispatch: Option<Dispatched>,
    }

    /// Drive one route end to end. `principal` false omits the human extension.
    async fn run(
        detail: DetailOutcome,
        scope: FakeScope,
        data_plane: FakeDataPlane,
        uri: &str,
        principal: bool,
    ) -> Reply {
        let calls = data_plane.calls.clone();
        let seen = data_plane.seen.clone();
        let mut request = Request::get(uri);
        if principal {
            request = request.extension(user(VIEWER));
        }
        let response = router(FakeDetail { outcome: detail }, scope, data_plane)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or(Value::String(String::from_utf8(bytes.to_vec()).unwrap()))
        };
        Reply {
            status,
            body,
            dispatch_calls: calls.load(SeqCst),
            last_dispatch: seen.lock().unwrap().last().cloned(),
        }
    }

    fn present(f: AgentDetailFacts) -> DetailOutcome {
        DetailOutcome::Present(Box::new(f))
    }

    fn sample_tree() -> WorkspaceTree {
        WorkspaceTree {
            nodes: vec![
                WorkspaceTreeNode {
                    path: "dir".to_string(),
                    kind: WorkspaceNodeKind::Directory,
                    size: JsSafeInteger::new(0).unwrap(),
                    mtime: silverhand_backend_core::workspace_data_plane::WorkspaceMtime::new(10.0)
                        .unwrap(),
                },
                WorkspaceTreeNode {
                    path: "dir/a.txt".to_string(),
                    kind: WorkspaceNodeKind::File,
                    size: JsSafeInteger::new(4).unwrap(),
                    mtime: silverhand_backend_core::workspace_data_plane::WorkspaceMtime::new(20.0)
                        .unwrap(),
                },
            ],
        }
    }

    fn sample_file() -> WorkspaceFileRead {
        WorkspaceFileRead {
            path: "a.txt".to_string(),
            content: "body".to_string(),
            size: JsSafeInteger::new(4).unwrap(),
            mtime: silverhand_backend_core::workspace_data_plane::WorkspaceMtime::new(20.0)
                .unwrap(),
            truncated: false,
        }
    }

    // ---- ladder ----

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_dispatches() {
        let reply = run(
            present(facts("a1", OWNER, HOST)),
            FakeScope::authorizing(),
            FakeDataPlane::ok(Answer::Tree(sample_tree())),
            "/api/agents/a1/workspace/tree",
            false,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.dispatch_calls, 0);
    }

    #[tokio::test]
    async fn an_absent_agent_is_the_anti_enumeration_404_with_no_code() {
        let reply = run(
            DetailOutcome::Absent,
            FakeScope::authorizing(),
            FakeDataPlane::ok(Answer::Tree(sample_tree())),
            "/api/agents/a1/workspace/tree",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.body, json!({ "error": "agent not found" }));
        assert_eq!(reply.dispatch_calls, 0);
    }

    #[tokio::test]
    async fn a_detail_read_failure_is_a_safe_500_that_leaks_no_cause() {
        let reply = run(
            DetailOutcome::Fails,
            FakeScope::authorizing(),
            FakeDataPlane::ok(Answer::Tree(sample_tree())),
            "/api/agents/a1/workspace/tree",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            reply.body,
            Value::String("Internal Server Error".to_string())
        );
    }

    #[tokio::test]
    async fn a_bundle_resolved_for_another_agent_is_a_safe_500() {
        let reply = run(
            present(facts("a2", OWNER, HOST)),
            FakeScope::authorizing(),
            FakeDataPlane::ok(Answer::Tree(sample_tree())),
            "/api/agents/a1/workspace/tree",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.dispatch_calls, 0);
    }

    #[tokio::test]
    async fn a_caller_outside_the_acl_is_the_same_404() {
        // `stranger` is in no viewer arm of this bundle.
        let reply = run(
            present(AgentDetailFacts::from_source(
                agent("a1"),
                user("owner-only"),
                [],
                [],
                MaterializationStatus::Active,
                projection(HOST),
            )),
            FakeScope::authorizing(),
            FakeDataPlane::ok(Answer::Tree(sample_tree())),
            "/api/agents/a1/workspace/tree",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.body, json!({ "error": "agent not found" }));
        assert_eq!(reply.dispatch_calls, 0);
    }

    #[tokio::test]
    async fn a_co_member_who_is_not_the_owner_is_forbidden() {
        // The viewer is admitted by the workspace arm but did NOT create the agent:
        // visibility passes, authority refuses with the 403 envelope.
        let reply = run(
            present(AgentDetailFacts::from_source(
                agent("a1"),
                user("someone-else"),
                [],
                [user(VIEWER)],
                MaterializationStatus::Active,
                projection(HOST),
            )),
            FakeScope::authorizing(),
            FakeDataPlane::ok(Answer::Tree(sample_tree())),
            "/api/agents/a1/workspace/tree",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(
            reply.body,
            json!({ "error": "forbidden", "code": "forbidden" })
        );
        assert_eq!(reply.dispatch_calls, 0);
    }

    // ---- home view + command discriminator ----

    #[tokio::test]
    async fn the_home_view_dispatches_the_tree_command_with_no_workspace() {
        let data_plane = FakeDataPlane::ok(Answer::Tree(sample_tree()));
        let reply = run(
            present(facts("a1", OWNER, HOST)),
            FakeScope::authorizing(),
            data_plane,
            "/api/agents/a1/workspace/tree",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.last_dispatch,
            Some(Dispatched {
                agent: "a1".to_string(),
                host: HOST.to_string(),
                workspace: None,
                path: None,
            })
        );
        assert_eq!(
            reply.body,
            json!({
                "tree": { "nodes": [
                    { "path": "dir", "kind": "directory", "size": 0, "mtime": 10.0 },
                    { "path": "dir/a.txt", "kind": "file", "size": 4, "mtime": 20.0 }
                ] }
            })
        );
    }

    #[tokio::test]
    async fn the_file_route_dispatches_the_file_command_with_the_opaque_path() {
        let data_plane = FakeDataPlane::ok(Answer::File(sample_file()));
        let reply = run(
            present(facts("a1", OWNER, HOST)),
            FakeScope::authorizing(),
            data_plane,
            "/api/agents/a1/workspace/file?path=%2Fweird%2F..%2Fkept-opaque",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        // The path crosses verbatim — no traversal normalization on our side.
        assert_eq!(
            reply.last_dispatch.unwrap().path.as_deref(),
            Some("/weird/../kept-opaque")
        );
        assert_eq!(
            reply.body,
            json!({
                "file": {
                    "path": "a.txt", "content": "body", "size": 4, "mtime": 20.0, "truncated": false
                }
            })
        );
    }

    #[tokio::test]
    async fn a_named_workspace_carries_its_scope_into_the_dispatch() {
        let data_plane = FakeDataPlane::ok(Answer::Tree(sample_tree()));
        let reply = run(
            present(facts("a1", OWNER, HOST)),
            FakeScope::authorizing(),
            data_plane,
            "/api/agents/a1/workspace/tree?workspace_id=ws-7",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.last_dispatch.unwrap().workspace.as_deref(),
            Some("ws-7")
        );
    }

    // ---- scope classification + refusals ----

    #[tokio::test]
    async fn an_empty_workspace_id_is_the_400_before_any_scope_read_or_dispatch() {
        let reply = run(
            present(facts("a1", OWNER, HOST)),
            FakeScope {
                fails: true,
                ..FakeScope::authorizing()
            },
            FakeDataPlane::ok(Answer::Tree(sample_tree())),
            "/api/agents/a1/workspace/tree?workspace_id=",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            reply.body,
            json!({ "error": "workspace_id query param must not be empty", "code": "invalid_workspace_id" })
        );
        // The scope reader would have errored had it run, proving the empty-string
        // classification short-circuits before it.
        assert_eq!(reply.dispatch_calls, 0);
    }

    #[tokio::test]
    async fn scope_refusals_render_their_distinct_envelopes() {
        let cases = [
            (
                FakeScope {
                    present: false,
                    ..FakeScope::authorizing()
                },
                StatusCode::NOT_FOUND,
                json!({ "error": "workspace not found", "code": "workspace_not_found" }),
            ),
            (
                FakeScope {
                    human_is_member: false,
                    ..FakeScope::authorizing()
                },
                StatusCode::FORBIDDEN,
                json!({ "error": "forbidden", "code": "forbidden" }),
            ),
            (
                FakeScope {
                    agent_is_member: false,
                    ..FakeScope::authorizing()
                },
                StatusCode::NOT_FOUND,
                json!({ "error": "agent is not a member of this workspace", "code": "workspace_agent_not_member" }),
            ),
        ];
        for (scope, status, body) in cases {
            let reply = run(
                present(facts("a1", OWNER, HOST)),
                scope,
                FakeDataPlane::ok(Answer::Tree(sample_tree())),
                "/api/agents/a1/workspace/tree?workspace_id=ws-7",
                true,
            )
            .await;
            assert_eq!(reply.status, status);
            assert_eq!(reply.body, body);
            assert_eq!(reply.dispatch_calls, 0);
        }
    }

    #[tokio::test]
    async fn a_scope_read_failure_is_a_safe_500() {
        let reply = run(
            present(facts("a1", OWNER, HOST)),
            FakeScope {
                fails: true,
                ..FakeScope::authorizing()
            },
            FakeDataPlane::ok(Answer::Tree(sample_tree())),
            "/api/agents/a1/workspace/tree?workspace_id=ws-7",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.dispatch_calls, 0);
    }

    // ---- source-binding the scope bundle to the request triple (P0) ----

    #[tokio::test]
    async fn each_scope_coordinate_is_bound_before_dispatch() {
        for scope in [
            FakeScope {
                stamp_workspace: Some(WorkspaceId::parse("ws-other").unwrap()),
                ..FakeScope::authorizing()
            },
            FakeScope {
                stamp_viewer: Some(user("someone-else")),
                ..FakeScope::authorizing()
            },
            FakeScope {
                stamp_agent: Some(agent("a2")),
                ..FakeScope::authorizing()
            },
        ] {
            let reply = run(
                present(facts("a1", OWNER, HOST)),
                scope,
                FakeDataPlane::ok(Answer::Tree(sample_tree())),
                "/api/agents/a1/workspace/tree?workspace_id=ws-7",
                true,
            )
            .await;
            assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(reply.dispatch_calls, 0);
        }
    }

    // ---- deferred query parse: duplicates + form decode (P1) ----

    #[tokio::test]
    async fn a_duplicate_workspace_id_takes_the_first_occurrence() {
        // A repeated key is NOT a native 400: the query is parsed after the authority
        // ladder and the first value wins, so the scope resolves for `ws-7`.
        let data_plane = FakeDataPlane::ok(Answer::Tree(sample_tree()));
        let reply = run(
            present(facts("a1", OWNER, HOST)),
            FakeScope::authorizing(),
            data_plane,
            "/api/agents/a1/workspace/tree?workspace_id=ws-7&workspace_id=ws-9",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.last_dispatch.unwrap().workspace.as_deref(),
            Some("ws-7")
        );
    }

    #[tokio::test]
    async fn a_non_viewer_with_a_duplicate_query_is_the_404_before_any_scope_read() {
        // A caller outside the ACL plus a duplicate `workspace_id`: with an eager
        // extractor the duplicate would 400 before visibility; here the query is
        // deferred, so the anti-enumeration 404 wins and the (failing) scope reader
        // is never consulted — proving no scope read precedes authority.
        let reply = run(
            present(AgentDetailFacts::from_source(
                agent("a1"),
                user("owner-only"),
                [],
                [],
                MaterializationStatus::Active,
                projection(HOST),
            )),
            FakeScope {
                fails: true,
                ..FakeScope::authorizing()
            },
            FakeDataPlane::ok(Answer::Tree(sample_tree())),
            "/api/agents/a1/workspace/tree?workspace_id=ws-7&workspace_id=ws-9",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.body, json!({ "error": "agent not found" }));
        assert_eq!(reply.dispatch_calls, 0);
    }

    #[tokio::test]
    async fn a_plus_and_percent_in_the_path_decode_before_dispatch() {
        // `form_urlencoded` semantics: `+` becomes a space and `%2F` a slash, matching
        // the TS decode. The decoded string then crosses to the daemon verbatim.
        let data_plane = FakeDataPlane::ok(Answer::File(sample_file()));
        let reply = run(
            present(facts("a1", OWNER, HOST)),
            FakeScope::authorizing(),
            data_plane,
            "/api/agents/a1/workspace/file?path=a+b%2Fc.txt",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.last_dispatch.unwrap().path.as_deref(),
            Some("a b/c.txt")
        );
    }

    // ---- path (file only), ordered after scope ----

    #[tokio::test]
    async fn a_missing_or_empty_path_is_400_forbidden_path() {
        for uri in [
            "/api/agents/a1/workspace/file",
            "/api/agents/a1/workspace/file?path=",
        ] {
            let reply = run(
                present(facts("a1", OWNER, HOST)),
                FakeScope::authorizing(),
                FakeDataPlane::ok(Answer::File(sample_file())),
                uri,
                true,
            )
            .await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(
                reply.body,
                json!({ "error": "path query param is required", "code": "forbidden_path" })
            );
            assert_eq!(reply.dispatch_calls, 0);
        }
    }

    #[tokio::test]
    async fn an_empty_workspace_id_outranks_a_missing_path() {
        // Both are wrong; the scope 400 wins because path is classified after scope.
        let reply = run(
            present(facts("a1", OWNER, HOST)),
            FakeScope::authorizing(),
            FakeDataPlane::ok(Answer::File(sample_file())),
            "/api/agents/a1/workspace/file?workspace_id=",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.body["code"], "invalid_workspace_id");
    }

    // ---- fault mapping ----

    #[tokio::test]
    async fn every_fault_code_maps_to_its_status_and_wire_code() {
        let cases = [
            (
                WorkspaceFsFaultCode::HostAttachPending,
                503,
                "host_attach_pending",
            ),
            (WorkspaceFsFaultCode::DaemonTimeout, 504, "daemon_timeout"),
            (
                WorkspaceFsFaultCode::DaemonInternalError,
                502,
                "daemon_internal_error",
            ),
            (WorkspaceFsFaultCode::FsUnavailable, 503, "fs_unavailable"),
            (
                WorkspaceFsFaultCode::RegistryUnavailable,
                503,
                "registry_unavailable",
            ),
            (WorkspaceFsFaultCode::ForbiddenPath, 400, "forbidden_path"),
            (WorkspaceFsFaultCode::NotFound, 404, "not_found"),
        ];
        for (code, status, wire) in cases {
            let reply = run(
                present(facts("a1", OWNER, HOST)),
                FakeScope::authorizing(),
                FakeDataPlane::faulting(code),
                "/api/agents/a1/workspace/tree",
                true,
            )
            .await;
            assert_eq!(reply.status.as_u16(), status, "{wire}");
            assert_eq!(reply.body["code"], wire);
            assert_eq!(reply.body["error"], "product message");
        }
    }
    #[tokio::test]
    async fn real_host_fractional_mtime_payload_survives_tree_and_file_http_routes() {
        // Captured from the real host handlers over an isolated file with a
        // fractional stat.mtimeMs. Feed the production codec, not invented facts.
        let payload: Value =
            serde_json::from_str(include_str!("fixtures/fractional_mtime.json")).unwrap();
        for session_scope in ["agent", "conversation"] {
            for (key, uri) in [
                ("tree", "/api/agents/a1/workspace/tree"),
                ("file", "/api/agents/a1/workspace/file?path=MEMORY.md"),
            ] {
                let decoded = if key == "tree" {
                    super::super::codec::decode_tree(&payload[key]).map(Answer::Tree)
                } else {
                    super::super::codec::decode_file(&payload[key]).map(Answer::File)
                };
                let plane = decoded.map(FakeDataPlane::ok).unwrap_or_else(|| {
                    FakeDataPlane::faulting(WorkspaceFsFaultCode::DaemonInternalError)
                });
                let mut detail = projection(HOST);
                detail.session_scope = session_scope.to_owned();
                let scoped_facts = AgentDetailFacts::from_source(
                    agent("a1"),
                    user(OWNER),
                    [],
                    [],
                    MaterializationStatus::Active,
                    detail,
                );
                let reply = run(
                    present(scoped_facts),
                    FakeScope::authorizing(),
                    plane,
                    uri,
                    true,
                )
                .await;
                assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
                assert_eq!(reply.body[key], payload[key][key]);
            }
        }
    }
}
