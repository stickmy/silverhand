//! Driver-free contract test for the agent-enrollment HTTP surface.
//!
//! One `FakeAccess` implements all three ports — the boundary
//! [`WorkspaceAccessPolicy`], the [`AgentEnrollmentReadPort`], and the
//! [`AgentEnrollmentUnitOfWork`] — and records every port call in a shared
//! trace. The tests pin exact status + semantic body for each product outcome
//! and, through the trace, that the two authorization doors run in order: the
//! request-boundary floor always first, and the application (agent read, then
//! write) only after it admits.

use std::{
    collections::HashMap,
    io::{self, Write},
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::foundation::identity::{AgentId, Principal, UserId, WorkspaceId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use silverhand_backend_core::identity_access::access::{
    AddHumanMemberError, AgentAuthorityFacts, AgentEnrollmentReadPort, AgentEnrollmentUnitOfWork,
    AgentWorkspaceMembership, Capability, EnrollmentClock, EnrollmentKey, HumanEnrollmentCommit,
    HumanEnrollmentKey, HumanEnrollmentReadPort, HumanEnrollmentUnitOfWork, HumanUser,
    ViewerAnnounceFailure, ViewerWorkspacesChangedSink, WorkspaceAccessPolicy, WorkspaceGovernance,
    WorkspaceRole, WorkspaceUserMembership,
};
use silverhand_backend_server::identity_access::access::{
    router,
    wire::{AGENT_NOT_OWNED_CODE, AGENT_NOT_OWNED_MESSAGE},
};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

const JOINED_AT_MS: i64 = 1_700_000_000_000;
const DRIVER_DETAIL: &str = "fake driver detail must stay private";

#[derive(Debug)]
struct DriverError;
impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{DRIVER_DETAIL}")
    }
}
impl std::error::Error for DriverError {}

#[derive(Clone)]
struct FakeAccess {
    governance: WorkspaceGovernance,
    policy_fails: bool,
    agents: HashMap<String, AgentAuthorityFacts>,
    enroll_fails: bool,
    users: HashMap<String, HumanUser>,
    /// The in-lock authority re-derivation refuses (a demotion in the seam).
    human_forbidden: bool,
    /// The human write hits a driver fault.
    human_enroll_fails: bool,
    /// The role and create-flag the human unit of work reports back — a create
    /// (`true`) announces, a re-enrollment (`false`) stays silent.
    human_role: WorkspaceRole,
    human_created: bool,
    /// The announce transport refuses. The membership is already committed when
    /// this fires, so it must change the LOG and nothing else.
    announce_fails: bool,
    trace: Arc<Mutex<Vec<&'static str>>>,
    memberships: Arc<Mutex<Vec<(String, String)>>>,
}

impl Default for FakeAccess {
    fn default() -> Self {
        Self {
            governance: WorkspaceGovernance::Admin,
            policy_fails: false,
            agents: HashMap::new(),
            enroll_fails: false,
            users: HashMap::new(),
            human_forbidden: false,
            human_enroll_fails: false,
            human_role: WorkspaceRole::Member,
            human_created: true,
            announce_fails: false,
            trace: Arc::new(Mutex::new(Vec::new())),
            memberships: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl FakeAccess {
    fn with_agent(mut self, id: &str, owner: &str) -> Self {
        self.agents.insert(
            id.to_owned(),
            AgentAuthorityFacts {
                id: AgentId::parse(id).unwrap(),
                created_by_user_id: UserId::parse(owner).unwrap(),
            },
        );
        self
    }

    fn with_user(mut self, user: HumanUser) -> Self {
        self.users.insert(user.id.as_str().to_owned(), user);
        self
    }

    fn governed(mut self, governance: WorkspaceGovernance) -> Self {
        self.governance = governance;
        self
    }

    fn trace(&self) -> Vec<&'static str> {
        self.trace.lock().unwrap().clone()
    }
}

impl WorkspaceAccessPolicy for FakeAccess {
    async fn authorize_member_governance(
        &self,
        _principal: &Principal,
        _workspace_id: &WorkspaceId,
    ) -> Result<WorkspaceGovernance, PersistenceFailure> {
        self.trace.lock().unwrap().push("policy");
        if self.policy_fails {
            return Err(PersistenceFailure::from_source(DriverError));
        }
        Ok(self.governance)
    }
}

impl AgentEnrollmentReadPort for FakeAccess {
    async fn agent(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<AgentAuthorityFacts>, PersistenceFailure> {
        self.trace.lock().unwrap().push("agent_read");
        Ok(self.agents.get(agent_id.as_str()).cloned())
    }
}

impl AgentEnrollmentUnitOfWork for FakeAccess {
    async fn enroll_agent(
        &self,
        key: EnrollmentKey,
        _seed_on_create: &[Capability],
        joined_at_candidate: i64,
    ) -> Result<AgentWorkspaceMembership, PersistenceFailure> {
        self.trace.lock().unwrap().push("enroll");
        if self.enroll_fails {
            return Err(PersistenceFailure::from_source(DriverError));
        }
        self.memberships.lock().unwrap().push((
            key.workspace_id.as_str().to_owned(),
            key.agent_id.as_str().to_owned(),
        ));
        Ok(AgentWorkspaceMembership {
            workspace_id: key.workspace_id,
            agent_id: key.agent_id,
            // Echo the candidate the application sampled from the clock, so the
            // success test observes the clock value flowing to the wire.
            joined_at_ms: joined_at_candidate,
        })
    }
}

impl EnrollmentClock for FakeAccess {
    fn now_unix_millis(&self) -> i64 {
        JOINED_AT_MS
    }
}

impl HumanEnrollmentReadPort for FakeAccess {
    async fn user(&self, user_id: &UserId) -> Result<Option<HumanUser>, PersistenceFailure> {
        self.trace.lock().unwrap().push("user_read");
        Ok(self.users.get(user_id.as_str()).cloned())
    }
}

impl HumanEnrollmentUnitOfWork for FakeAccess {
    async fn enroll_human(
        &self,
        key: HumanEnrollmentKey,
        _actor: Principal,
        joined_at_candidate: i64,
    ) -> Result<HumanEnrollmentCommit, AddHumanMemberError> {
        self.trace.lock().unwrap().push("human_enroll");
        if self.human_forbidden {
            // The in-lock re-derivation rolled back; nothing was written.
            return Err(AddHumanMemberError::Forbidden);
        }
        if self.human_enroll_fails {
            return Err(AddHumanMemberError::Persistence(
                PersistenceFailure::from_source(DriverError),
            ));
        }
        Ok(HumanEnrollmentCommit {
            member: WorkspaceUserMembership {
                workspace_id: key.workspace_id,
                user_id: key.user_id,
                // Echo the sampled candidate so the success test sees the clock
                // value reach the wire; a real re-enrollment would re-read the row.
                joined_at_ms: joined_at_candidate,
                role: self.human_role,
            },
            created: self.human_created,
        })
    }
}

impl ViewerWorkspacesChangedSink for FakeAccess {
    async fn viewer_added_to_workspace(
        &self,
        _user_id: &UserId,
        _workspace_id: &WorkspaceId,
        _role: WorkspaceRole,
    ) -> Result<(), ViewerAnnounceFailure> {
        self.trace.lock().unwrap().push("sink");
        if self.announce_fails {
            return Err(ViewerAnnounceFailure::from_source(DriverError));
        }
        Ok(())
    }
}

fn app(fake: &FakeAccess) -> Router {
    router(fake.clone(), fake.clone(), fake.clone(), fake.clone())
}

fn request(workspace_id: &str, viewer: &str, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("/api/workspaces/{workspace_id}/members"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    // Exactly what `require_human_session` leaves behind: the sole `UserId`.
    // The route constructs the application principal from it.
    request
        .extensions_mut()
        .insert(UserId::parse(viewer).unwrap());
    request
}

async fn send(app: Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

/// Like [`send`] but returns the raw `Content-Type` and body bytes, for the
/// unmapped-500 path whose body is bare `text/plain`, not JSON.
async fn send_raw(app: Router, request: Request<Body>) -> (StatusCode, Option<String>, String) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .map(|value| value.to_str().unwrap().to_owned());
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

#[tokio::test]
async fn admitted_owner_enrolls_and_the_two_doors_run_in_order() {
    let fake = FakeAccess::default().with_agent("agent-1", "user-1");

    let (status, body) = send(
        app(&fake),
        request("ws-1", "user-1", r#"{"agent_id":"agent-1"}"#),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        body,
        json!({"member": {"workspace_id": "ws-1", "agent_id": "agent-1", "joined_at": JOINED_AT_MS}})
    );
    assert_eq!(fake.trace(), ["policy", "agent_read", "enroll"]);
    assert_eq!(fake.memberships.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn tagged_agent_body_is_accepted_like_the_bare_form() {
    let fake = FakeAccess::default().with_agent("agent-1", "user-1");

    let (status, _) = send(
        app(&fake),
        request("ws-1", "user-1", r#"{"kind":"agent","agent_id":"agent-1"}"#),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn the_floor_refuses_a_valid_body_before_the_agent_is_read() {
    let fake = FakeAccess::default().governed(WorkspaceGovernance::Denied);

    // A structurally valid body: the native decode passes, and the floor is the
    // next door — it answers `403` before the body is classified or the agent is
    // read, so a non-member still cannot probe past the boundary.
    let (status, body) = send(
        app(&fake),
        request("ws-1", "user-1", r#"{"agent_id":"agent-1"}"#),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"error": "forbidden"}));
    assert_eq!(fake.trace(), ["policy"]);
}

/// The ratified floor-before-body ordering: the governance floor runs BEFORE the
/// body is decoded, so a non-member is answered `403` whatever the body is — even
/// a body the native `Json` extractor could not decode. The body is never
/// buffered, so the caller cannot probe the body verdict. This is the base
/// route's `body: Bytes` contract, preserved through the DEFERRED
/// `Json::<ObjectOnly<_>>` decode.
#[tokio::test]
async fn a_non_member_is_forbidden_before_a_malformed_body_is_decoded() {
    let fake = FakeAccess::default().governed(WorkspaceGovernance::Denied);

    let (status, body) = send(app(&fake), request("ws-1", "user-1", "not json")).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"error": "forbidden"}));
    assert_eq!(
        fake.trace(),
        ["policy"],
        "the floor answers before the body is decoded, malformed or not"
    );
}

/// A value-shaped array from a NON-MEMBER is likewise answered by the floor, not
/// the decoder: the body verdict stays unprobeable. A derived struct visitor
/// would fill `AddMemberRaw` (kind, agent_id, user_id) positionally from
/// `["agent", "agent-1", null]`, but decode never even runs here.
#[tokio::test]
async fn a_non_member_is_forbidden_before_a_value_shaped_array_is_decoded() {
    let fake = FakeAccess::default().governed(WorkspaceGovernance::Denied);

    let (status, body) = send(
        app(&fake),
        request("ws-1", "user-1", r#"["agent","agent-1",null]"#),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"error": "forbidden"}));
    assert_eq!(
        fake.trace(),
        ["policy"],
        "a non-member's array body is refused by the floor, never decoded"
    );
}

/// For an ADMITTED caller the shared `ObjectOnly<T>` map floor catches the
/// value-shaped array: a top-level array whose elements line up by
/// type/value/order with `AddMemberRaw` would positionally classify as a
/// tagged-agent enrollment, but the object floor makes it a native structural
/// `422` at the deferred decode — after the governance floor, before the agent is
/// ever read, so no enrollment or write reaches the ports.
#[tokio::test]
async fn an_admitted_value_shaped_array_is_a_native_reject_with_zero_enrollment_reach() {
    let fake = FakeAccess::default();

    let response = app(&fake)
        .oneshot(request("ws-1", "user-1", r#"["agent","agent-1",null]"#))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        fake.trace(),
        ["policy"],
        "the array is rejected at decode after the floor; the agent is never read and nothing is enrolled"
    );
}

#[tokio::test]
async fn unknown_workspace_is_not_found_at_the_boundary() {
    let fake = FakeAccess::default().governed(WorkspaceGovernance::WorkspaceNotFound);

    let (status, body) = send(
        app(&fake),
        request("ghost", "user-1", r#"{"agent_id":"agent-1"}"#),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "workspace not found"}));
    assert_eq!(fake.trace(), ["policy"]);
}

#[tokio::test]
async fn a_structurally_valid_body_no_arm_accepts_is_invalid_input_after_the_floor() {
    let fake = FakeAccess::default();

    // `{}` decodes fine (every field is optional) but matches no product arm, so
    // it clears the native decode, meets the floor, is admitted, and only then
    // fails classification: the minimal semantic `422`.
    let (status, body) = send(app(&fake), request("ws-1", "user-1", "{}")).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body, json!({"error": "invalid input"}));
    // Admitted, then classification failed — the agent was never read.
    assert_eq!(fake.trace(), ["policy"]);
}

#[tokio::test]
async fn missing_agent_is_not_found_after_admission() {
    let fake = FakeAccess::default();

    let (status, body) = send(
        app(&fake),
        request("ws-1", "user-1", r#"{"agent_id":"ghost"}"#),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    // No `code` key on this refusal.
    assert_eq!(body, json!({"error": "agent not found"}));
    assert_eq!(fake.trace(), ["policy", "agent_read"]);
}

/// The product's `agent_id` is a plain string with no minimum length, so
/// `{"agent_id":""}` *classifies* — it is a well-formed agent request naming an
/// id no agent can have. The answer is therefore the same exact `404` as any
/// other miss, never `400 invalid input`: the body was valid, the agent is not
/// there. Because the domain's id type refuses the value before the port, this
/// `404` is reached with the read and the write both untouched.
#[tokio::test]
async fn an_empty_agent_id_is_the_exact_agent_not_found_without_touching_the_ports() {
    let fake = FakeAccess::default();

    let (status, body) = send(app(&fake), request("ws-1", "user-1", r#"{"agent_id":""}"#)).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "agent not found"}));
    assert_eq!(
        fake.trace(),
        ["policy"],
        "an unusable id is decided at the wire: neither the read port nor the unit of work runs"
    );
}

/// The human arm of the same rule: `{"kind":"human","user_id":""}` classifies as
/// a human request, clears the admin floor, and then earns the exact
/// `user not found` — not `invalid input` — with the user read never entered.
#[tokio::test]
async fn an_empty_user_id_is_the_exact_user_not_found_without_touching_the_ports() {
    let fake = FakeAccess::default().with_user(invitee());

    let (status, body) = send(
        app(&fake),
        request("ws-1", "admin", r#"{"kind":"human","user_id":""}"#),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "user not found"}));
    assert_eq!(
        fake.trace(),
        ["policy"],
        "an unusable id is decided at the wire: the user read never runs"
    );
}

/// A route composed without the human-session perimeter has no identity to act
/// as. That is a wiring fault, not a request the product can answer, so it takes
/// the lane's safe `500` — logged once, and before the policy is ever consulted,
/// so a missing perimeter can never be mistaken for an authorization outcome.
#[tokio::test]
async fn a_route_reached_without_the_perimeter_is_a_logged_safe_500_before_any_door() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let fake = FakeAccess::default().with_agent("agent-1", "user-1");
    let mut unauthenticated = Request::builder()
        .method("POST")
        .uri("/api/workspaces/ws-1/members")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"agent_id":"agent-1"}"#))
        .unwrap();
    unauthenticated.extensions_mut().clear();

    let (status, content_type, body) = send_raw(app(&fake), unauthenticated).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(content_type.as_deref(), Some("text/plain; charset=utf-8"));
    assert_eq!(body, "Internal Server Error");
    assert_eq!(
        fake.trace(),
        Vec::<&str>::new(),
        "no identity means no door runs at all"
    );

    let errors: Vec<Value> = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "workspace member route reached without its human principal fact"
    );
}

#[tokio::test]
async fn foreign_agent_is_refused_with_the_exact_ownership_body() {
    let fake = FakeAccess::default().with_agent("agent-1", "owner");

    let (status, body) = send(
        app(&fake),
        request("ws-1", "intruder", r#"{"agent_id":"agent-1"}"#),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        json!({"error": AGENT_NOT_OWNED_MESSAGE, "code": AGENT_NOT_OWNED_CODE})
    );
    assert_eq!(fake.trace(), ["policy", "agent_read"]);
}

#[tokio::test]
async fn persistence_failure_in_the_write_returns_a_safe_500_and_logs_one_chain() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let fake = FakeAccess {
        enroll_fails: true,
        ..FakeAccess::default().with_agent("agent-1", "user-1")
    };

    let (status, content_type, body) = send_raw(
        app(&fake),
        request("ws-1", "user-1", r#"{"agent_id":"agent-1"}"#),
    )
    .await;

    // The product route registers no onError, so an unmapped failure is the
    // framework default: 500 with a bare text/plain "Internal Server Error"
    // body — never a JSON envelope — and no driver detail leaks into it.
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        content_type.as_deref(),
        Some("text/plain; charset=utf-8"),
        "unmapped 500 is plain text, not JSON"
    );
    assert_eq!(body, "Internal Server Error");
    assert!(!body.contains(DRIVER_DETAIL));
    assert_eq!(fake.trace(), ["policy", "agent_read", "enroll"]);

    let errors: Vec<Value> = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "request failed at the persistence boundary"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains(DRIVER_DETAIL));
}

/// A person's display identity for the human-enrollment tests. `avatar_url` is
/// supplied already-derived: its derivation from the storage key is the SQLite
/// adapter's concern and pinned there, so here it only has to reach the wire.
fn invitee() -> HumanUser {
    HumanUser {
        id: UserId::parse("invitee").unwrap(),
        name: "invitee-name".to_owned(),
        display_name: "Invitee".to_owned(),
        avatar_url: Some("/api/users/invitee/avatar?v=99".to_owned()),
        created_at: 10,
        updated_at: 99,
    }
}

const HUMAN_BODY: &str = r#"{"kind":"human","user_id":"invitee"}"#;

#[tokio::test]
async fn a_member_ceiling_admits_the_agent_branch() {
    let fake = FakeAccess::default()
        .governed(WorkspaceGovernance::Member)
        .with_agent("agent-1", "user-1");

    let (status, _) = send(
        app(&fake),
        request("ws-1", "user-1", r#"{"agent_id":"agent-1"}"#),
    )
    .await;

    // The agent branch needs only the member floor, which `Member` clears.
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(fake.trace(), ["policy", "agent_read", "enroll"]);
}

#[tokio::test]
async fn a_member_ceiling_refuses_the_human_branch_before_any_read() {
    let fake = FakeAccess::default()
        .governed(WorkspaceGovernance::Member)
        .with_user(invitee());

    let (status, body) = send(app(&fake), request("ws-1", "plain", HUMAN_BODY)).await;

    // The people roster needs the admin floor; a plain member is refused by the
    // pure comparison, so neither the user read nor the write ever runs.
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"error": "forbidden"}));
    assert_eq!(fake.trace(), ["policy"]);
}

#[tokio::test]
async fn an_admin_enrolls_a_human_and_returns_the_merged_member() {
    let fake = FakeAccess::default().with_user(invitee());

    let (status, body) = send(app(&fake), request("ws-1", "admin", HUMAN_BODY)).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        body,
        json!({
            "member": {
                "workspace_id": "ws-1",
                "user_id": "invitee",
                "joined_at": JOINED_AT_MS,
                "role": "member",
            },
            "human_member": {
                "id": "invitee",
                "name": "invitee-name",
                "display_name": "Invitee",
                "avatar_url": "/api/users/invitee/avatar?v=99",
                "created_at": 10,
                "updated_at": 99,
                "workspace_id": "ws-1",
                "user_id": "invitee",
                "joined_at": JOINED_AT_MS,
                "role": "member",
            }
        })
    );
    // Existence read, then the atomic write, then the exactly-once announce.
    assert_eq!(
        fake.trace(),
        ["policy", "user_read", "human_enroll", "sink"]
    );
}

#[tokio::test]
async fn enrolling_an_absent_user_is_not_found_without_a_write() {
    let fake = FakeAccess::default();

    let (status, body) = send(app(&fake), request("ws-1", "admin", HUMAN_BODY)).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "user not found"}));
    // "No such user" is answered by the read; the write lock is never taken.
    assert_eq!(fake.trace(), ["policy", "user_read"]);
}

#[tokio::test]
async fn in_lock_authority_loss_is_forbidden_and_announces_nothing() {
    let fake = FakeAccess {
        human_forbidden: true,
        ..FakeAccess::default().with_user(invitee())
    };

    let (status, body) = send(app(&fake), request("ws-1", "admin", HUMAN_BODY)).await;

    // The boundary admitted an admin, but the in-lock re-derivation caught
    // authority lost in the seam: a typed 403, and no announce after a rollback.
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"error": "forbidden"}));
    assert_eq!(fake.trace(), ["policy", "user_read", "human_enroll"]);
}

#[tokio::test]
async fn a_re_enrollment_succeeds_but_announces_nothing() {
    let fake = FakeAccess {
        human_created: false,
        human_role: WorkspaceRole::Owner,
        ..FakeAccess::default().with_user(invitee())
    };

    let (status, body) = send(app(&fake), request("ws-1", "admin", HUMAN_BODY)).await;

    assert_eq!(status, StatusCode::CREATED);
    // The idempotent write kept the original role; no demotion to member.
    assert_eq!(body["member"]["role"], "owner");
    // A re-enrollment did not create, so the sink is never called.
    assert_eq!(fake.trace(), ["policy", "user_read", "human_enroll"]);
}

#[tokio::test]
async fn a_human_write_failure_travels_the_same_seam_and_logs_one_chain() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let fake = FakeAccess {
        human_enroll_fails: true,
        ..FakeAccess::default().with_user(invitee())
    };

    let (status, content_type, body) =
        send_raw(app(&fake), request("ws-1", "admin", HUMAN_BODY)).await;

    // A persistence failure on the human write is not the human branch's to
    // render: it surfaces the same `AccessError` the agent branch does, so the
    // one lane-local seam produces the identical bare `text/plain` 500 and no
    // driver detail leaks — the branches share a single top-level renderer.
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(content_type.as_deref(), Some("text/plain; charset=utf-8"));
    assert_eq!(body, "Internal Server Error");
    assert!(!body.contains(DRIVER_DETAIL));
    // The failure struck inside the write, after the existence read; a rollback
    // means the create-only announce never runs.
    assert_eq!(fake.trace(), ["policy", "user_read", "human_enroll"]);

    // Exactly one ERROR chain — the seam logs once; the route does not re-log.
    let errors: Vec<Value> = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "request failed at the persistence boundary"
    );
    assert!(
        errors[0]["fields"]["error_chain"]
            .as_str()
            .unwrap()
            .contains(DRIVER_DETAIL)
    );
}

/// A lost announce is an OBSERVABILITY event, not a product failure: the
/// membership is committed, so the client is told `201`, and the one thing that
/// changes is that somebody can find out the viewer went untold.
///
/// Three facts at once, and each is a distinct way this could go wrong: the
/// response must not degrade (a `500` here would discard a committed write), the
/// full cause chain must reach the log (an opaque "announce failed" line would
/// send an operator hunting the wrong layer), and it must be logged EXACTLY once
/// (both the route and the error seam could plausibly claim this line; only one
/// may).
#[tokio::test]
async fn a_lost_announce_still_answers_created_and_logs_one_full_chain() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let fake = FakeAccess {
        announce_fails: true,
        ..FakeAccess::default().with_user(invitee())
    };

    let (status, body) = send(app(&fake), request("ws-1", "admin", HUMAN_BODY)).await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "the membership committed; a transport that could not deliver its notice \
         cannot retract that"
    );
    assert_eq!(body["member"]["role"], "member");
    // The sink WAS reached — "called and failed" is not "never called".
    assert_eq!(
        fake.trace(),
        ["policy", "user_read", "human_enroll", "sink"]
    );

    let errors: Vec<Value> = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect();
    assert_eq!(errors.len(), 1, "the loss is reported once, by one owner");
    assert_eq!(
        errors[0]["fields"]["message"],
        "the new workspace membership was committed but its viewer announce was lost"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(
        chain.contains("viewer workspace announce could not be delivered"),
        "the chain names the announce failure: {chain}"
    );
    assert!(
        chain.contains(DRIVER_DETAIL),
        "the transport's own reason survives to the log: {chain}"
    );
}

/// A re-enrollment is silent on BOTH channels, and the second half is the one a
/// naive implementation gets wrong: it is easy to log "announce skipped" for
/// every idempotent retry, which then makes a real loss unfindable in a log
/// search. `NotOwed` must produce no line at all.
#[tokio::test]
async fn a_re_enrollment_neither_announces_nor_logs() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // The sink is ALSO armed to fail, so "no log" cannot be explained away by a
    // sink that happened to succeed: nothing was owed, so nothing was attempted.
    let fake = FakeAccess {
        human_created: false,
        human_role: WorkspaceRole::Owner,
        announce_fails: true,
        ..FakeAccess::default().with_user(invitee())
    };

    let (status, body) = send(app(&fake), request("ws-1", "admin", HUMAN_BODY)).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["member"]["role"], "owner");
    assert_eq!(fake.trace(), ["policy", "user_read", "human_enroll"]);
    assert!(
        logs.events().is_empty(),
        "a re-enrollment owes no announce, so it writes no line: {:?}",
        logs.events()
    );
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);
impl LogBuffer {
    fn events(&self) -> Vec<Value> {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

struct LogWriter(LogBuffer);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.clone())
    }
}
