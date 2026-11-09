//! Handler tests for `GET /api/agents/{id}/runtime-state`: the fixed
//! visibility→payload order, the anti-enumeration 404 collapse, the source-bind
//! seal, and — the part unique to this face — the post-snapshot, exact-generation
//! live-focus guard and its fail-closed port faults.
//!
//! The three ports are faked and every read is counted, so a path that resolved a
//! value WITHOUT consulting a port (a refused caller, a generation mismatch) is
//! distinguishable from one that read and then computed.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_ref::ConversationRosterPolicy,
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::agent_runtime_state_read::{
        AgentRuntimeStateFacts, AgentRuntimeStateReadModel, RuntimeParticipationFact,
        RuntimeSeatFact, RuntimeSeatScopeFact,
    },
    host_runtime::runtime_credential::{
        AgentSlotPresentationFocusPort, RuntimeCustody, RuntimeCustodyReadPort, RuntimeGeneration,
        RuntimePrincipal, RuntimeScope,
    },
    host_runtime::runtime_event::RuntimeObservedStatus,
    identity_access::agent_lifecycle::MaterializationStatus,
};
use tower::ServiceExt;

use super::router;

const PATH: &str = "/api/agents/a1/runtime-state";

fn user(id: &str) -> UserId {
    UserId::parse(id).expect("user id")
}

fn agent(id: &str) -> AgentId {
    AgentId::parse(id).expect("agent id")
}

fn conversation(id: &str) -> ConversationId {
    ConversationId::parse(id).expect("conversation id")
}

fn generation(value: i64) -> RuntimeGeneration {
    RuntimeGeneration::new(value).expect("generation")
}

/// One readable, non-archived workspace participation on `c-open`.
fn readable_participation() -> RuntimeParticipationFact {
    RuntimeParticipationFact {
        conversation: conversation("c-open"),
        workspace_id: Some("w1".to_owned()),
        workspace_name: Some("Workspace One".to_owned()),
        placement_kind: "workspace".to_owned(),
        roster_policy: ConversationRosterPolicy::Open,
        channel_title: "General".to_owned(),
        archived: false,
        viewer_can_read: true,
    }
}

/// An agent-scope seat at the given generation, active, with an observed process.
fn agent_seat(seat_generation: i64) -> RuntimeSeatFact {
    RuntimeSeatFact {
        scope: RuntimeSeatScopeFact::Agent,
        generation: generation(seat_generation),
        status: RuntimeObservedStatus::Active,
        pid: Some(999),
        cli_session_id: Some("cli-a".to_owned()),
        queue_depth: 4,
    }
}

/// A runtime-state bundle for agent `id` on host-independent facts, whose creator
/// arm admits `creator` (pass the viewer's own id to admit it, another id to refuse
/// it) at the given stage, carrying one readable participation and one agent-scope
/// seat at `seat_generation`. The bundle is sealed to the decoded id `id`.
fn facts(
    id: &str,
    creator: &str,
    status: MaterializationStatus,
    seat_generation: i64,
) -> AgentRuntimeStateFacts {
    AgentRuntimeStateFacts::from_source(
        agent(id),
        user(creator),
        [],
        [],
        status,
        vec![readable_participation()],
        vec![agent_seat(seat_generation)],
    )
}

/// The ordinary admitting bundle: agent `a1`, viewer `viewer-1` is the creator,
/// active stage, agent-scope seat at generation 5.
fn admitting_facts() -> AgentRuntimeStateFacts {
    facts("a1", "viewer-1", MaterializationStatus::Active, 5)
}

#[derive(Clone)]
enum ReadOutcome {
    Present(AgentRuntimeStateFacts),
    Absent,
    Fails,
}

#[derive(Clone)]
enum VisibilityOutcome {
    Readable(bool),
    Fails,
}

#[derive(Clone)]
struct FakeReads {
    read: ReadOutcome,
    visibility: VisibilityOutcome,
    read_calls: Arc<AtomicUsize>,
    visibility_calls: Arc<AtomicUsize>,
}

impl AgentRuntimeStateReadModel for FakeReads {
    async fn read_agent_runtime_state(
        &self,
        _agent: &AgentId,
        _viewer: &UserId,
    ) -> Result<Option<AgentRuntimeStateFacts>, PersistenceFailure> {
        self.read_calls.fetch_add(1, SeqCst);
        match &self.read {
            ReadOutcome::Present(facts) => Ok(Some(facts.clone())),
            ReadOutcome::Absent => Ok(None),
            ReadOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                "snapshot gone",
            ))),
        }
    }

    async fn read_conversation_visibility(
        &self,
        _viewer: &UserId,
        _conversation: &ConversationId,
    ) -> Result<bool, PersistenceFailure> {
        self.visibility_calls.fetch_add(1, SeqCst);
        match &self.visibility {
            VisibilityOutcome::Readable(readable) => Ok(*readable),
            VisibilityOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                "visibility read gone",
            ))),
        }
    }
}

#[derive(Clone)]
enum CustodyOutcome {
    /// A live principal owns the agent slot at the given generation.
    Present(i64),
    Absent,
    Fails,
}

#[derive(Clone)]
struct FakeCustody {
    outcome: CustodyOutcome,
    calls: Arc<AtomicUsize>,
}

impl RuntimeCustodyReadPort for FakeCustody {
    async fn current(
        &self,
        scope: RuntimeScope,
    ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
        self.calls.fetch_add(1, SeqCst);
        match &self.outcome {
            CustodyOutcome::Present(gen_value) => {
                let custody = RuntimeCustody::new(
                    generation(*gen_value),
                    DaemonInstanceId::parse("d1").expect("daemon id"),
                );
                Ok(Some(RuntimePrincipal::for_scope(scope, custody)))
            }
            CustodyOutcome::Absent => Ok(None),
            CustodyOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                "custody read gone",
            ))),
        }
    }
}

#[derive(Clone)]
enum FocusOutcome {
    Focused(ConversationId),
    Unfocused,
    Fails,
}

#[derive(Clone)]
struct FakeFocus {
    outcome: FocusOutcome,
    calls: Arc<AtomicUsize>,
}

impl AgentSlotPresentationFocusPort for FakeFocus {
    async fn agent_slot_presentation_focus(
        &self,
        _principal: RuntimePrincipal,
    ) -> Result<Option<ConversationId>, PersistenceFailure> {
        self.calls.fetch_add(1, SeqCst);
        match &self.outcome {
            FocusOutcome::Focused(conversation) => Ok(Some(conversation.clone())),
            FocusOutcome::Unfocused => Ok(None),
            FocusOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                "focus read gone",
            ))),
        }
    }
}

struct Reply {
    status: StatusCode,
    body: String,
    read_calls: usize,
    visibility_calls: usize,
    custody_calls: usize,
    focus_calls: usize,
}

#[allow(clippy::too_many_arguments)]
async fn get(
    read: ReadOutcome,
    visibility: VisibilityOutcome,
    custody: CustodyOutcome,
    focus: FocusOutcome,
    with_principal: bool,
) -> Reply {
    let read_calls = Arc::new(AtomicUsize::new(0));
    let visibility_calls = Arc::new(AtomicUsize::new(0));
    let custody_calls = Arc::new(AtomicUsize::new(0));
    let focus_calls = Arc::new(AtomicUsize::new(0));
    let reads = FakeReads {
        read,
        visibility,
        read_calls: read_calls.clone(),
        visibility_calls: visibility_calls.clone(),
    };
    let custody = FakeCustody {
        outcome: custody,
        calls: custody_calls.clone(),
    };
    let focus = FakeFocus {
        outcome: focus,
        calls: focus_calls.clone(),
    };
    let mut request = Request::get(PATH);
    if with_principal {
        request = request.extension(user("viewer-1"));
    }
    let response = router(reads, custody, focus)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Reply {
        status,
        body: String::from_utf8(body.to_vec()).unwrap(),
        read_calls: read_calls.load(SeqCst),
        visibility_calls: visibility_calls.load(SeqCst),
        custody_calls: custody_calls.load(SeqCst),
        focus_calls: focus_calls.load(SeqCst),
    }
}

/// The whole projection for an admitted caller: a readable participation with every
/// wire key, and the agent seat carrying a live focus named on a readable conversation
/// — resolved through all three ports exactly once.
#[tokio::test]
async fn an_admitted_caller_sees_the_projected_state() {
    let reply = get(
        ReadOutcome::Present(admitting_facts()),
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Present(5),
        FocusOutcome::Focused(conversation("c-open")),
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&reply.body).unwrap(),
        json!({
            "participations": [{
                "conversation_id": "c-open",
                "workspace_id": "w1",
                "workspace_name": "Workspace One",
                "conversation_placement_kind": "workspace",
                "conversation_roster_policy": "open",
                "channel_id": "c-open",
                "channel_title": "General",
                "closed_at": null,
            }],
            "visible_participations": [{
                "conversation_id": "c-open",
                "workspace_id": "w1",
                "workspace_name": "Workspace One",
                "conversation_placement_kind": "workspace",
                "conversation_roster_policy": "open",
                "channel_id": "c-open",
                "channel_title": "General",
                "closed_at": null,
            }],
            "seats": [{
                "scope": { "kind": "agent" },
                "generation": 5,
                "status": "active",
                "pid": 999,
                "cli_session_id": "cli-a",
                "focus": { "kind": "conversation", "conversation_id": "c-open" },
                "queue_depth": 4,
            }],
        })
    );
    assert_eq!(reply.read_calls, 1);
    assert_eq!(reply.custody_calls, 1);
    assert_eq!(reply.focus_calls, 1);
    assert_eq!(reply.visibility_calls, 1);
}

/// A live focus on a conversation the caller may NOT read is revealed as `other`:
/// the agent is focused somewhere, but the wire never names where.
#[tokio::test]
async fn an_unreadable_live_focus_is_other() {
    let reply = get(
        ReadOutcome::Present(admitting_facts()),
        VisibilityOutcome::Readable(false),
        CustodyOutcome::Present(5),
        FocusOutcome::Focused(conversation("c-secret")),
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["seats"][0]["focus"], json!({ "kind": "other" }));
    // The focus conversation's readability WAS consulted, once.
    assert_eq!(reply.visibility_calls, 1);
}

/// STALE GENERATION: a live principal at generation 6 raced ahead of this snapshot's
/// generation-5 seat, so its focus is IGNORED — the seat's focus is null and the
/// focus port is NEVER consulted, so a newer generation's focus can never be spliced
/// onto the stale seat.
#[tokio::test]
async fn a_raced_generation_drops_the_focus_and_never_reads_it() {
    let reply = get(
        ReadOutcome::Present(admitting_facts()),
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Present(6),
        FocusOutcome::Focused(conversation("c-open")),
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["seats"][0]["focus"], Value::Null);
    assert_eq!(reply.custody_calls, 1);
    assert_eq!(
        reply.focus_calls, 0,
        "a mismatched generation never reads focus"
    );
    assert_eq!(reply.visibility_calls, 0);
}

/// No live process owns the agent slot ⇒ the seat's focus is null and the focus port
/// is never consulted.
#[tokio::test]
async fn no_live_principal_is_a_null_focus() {
    let reply = get(
        ReadOutcome::Present(admitting_facts()),
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Absent,
        FocusOutcome::Focused(conversation("c-open")),
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["seats"][0]["focus"], Value::Null);
    assert_eq!(reply.custody_calls, 1);
    assert_eq!(reply.focus_calls, 0);
}

/// A generation-matched live principal that reports NO focused conversation ⇒ null
/// focus, and the focus conversation's readability is never asked.
#[tokio::test]
async fn a_live_principal_with_no_focus_is_a_null_focus() {
    let reply = get(
        ReadOutcome::Present(admitting_facts()),
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Present(5),
        FocusOutcome::Unfocused,
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["seats"][0]["focus"], Value::Null);
    assert_eq!(reply.focus_calls, 1);
    assert_eq!(reply.visibility_calls, 0);
}

/// An absent agent is the ACL's absence arm: the anti-enumeration 404, and NO live
/// focus port is touched.
#[tokio::test]
async fn an_absent_agent_is_the_anti_enumeration_404() {
    let reply = get(
        ReadOutcome::Absent,
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Present(5),
        FocusOutcome::Focused(conversation("c-open")),
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_str::<Value>(&reply.body).unwrap(),
        json!({ "error": "agent not found" })
    );
    assert_eq!(reply.read_calls, 1);
    assert_eq!(reply.custody_calls, 0);
    assert_eq!(reply.focus_calls, 0);
}

/// A caller in NO visibility arm collapses to the SAME 404 an absent agent gets, and
/// no live focus is read — access precedes product.
#[tokio::test]
async fn a_viewer_in_no_arm_is_the_same_404() {
    let reply = get(
        // Creator is someone else; `viewer-1` is admitted through no arm.
        ReadOutcome::Present(facts(
            "a1",
            "someone-else",
            MaterializationStatus::Active,
            5,
        )),
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Present(5),
        FocusOutcome::Focused(conversation("c-open")),
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_str::<Value>(&reply.body).unwrap(),
        json!({ "error": "agent not found" })
    );
    assert_eq!(reply.custody_calls, 0);
    assert_eq!(reply.focus_calls, 0);
}

/// A row whose stage the active-only view hides is the SAME 404 for a caller who IS
/// in the ACL — indistinguishable from absence.
#[tokio::test]
async fn a_hidden_stage_is_the_same_404() {
    for status in [
        MaterializationStatus::PendingMaterialization,
        MaterializationStatus::Failed,
    ] {
        let reply = get(
            ReadOutcome::Present(facts("a1", "viewer-1", status, 5)),
            VisibilityOutcome::Readable(true),
            CustodyOutcome::Present(5),
            FocusOutcome::Focused(conversation("c-open")),
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(
            serde_json::from_str::<Value>(&reply.body).unwrap(),
            json!({ "error": "agent not found" })
        );
        assert_eq!(reply.custody_calls, 0);
    }
}

/// A bundle resolved for another agent than the one asked for is a composition defect
/// — the safe 500, decided BEFORE the gate, reading no live focus.
#[tokio::test]
async fn a_bundle_for_another_agent_is_a_safe_500() {
    let reply = get(
        // Sealed to `a2`, but the URL asks about `a1`; the creator arm would admit.
        ReadOutcome::Present(facts("a2", "viewer-1", MaterializationStatus::Active, 5)),
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Present(5),
        FocusOutcome::Focused(conversation("c-open")),
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.body, "Internal Server Error");
    assert_eq!(reply.custody_calls, 0);
    assert_eq!(reply.focus_calls, 0);
}

/// No principal reached the handler: refuse fail-closed BEFORE touching any port.
#[tokio::test]
async fn a_missing_principal_is_a_safe_500_that_never_reads() {
    let reply = get(
        ReadOutcome::Present(admitting_facts()),
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Present(5),
        FocusOutcome::Focused(conversation("c-open")),
        false,
    )
    .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.body, "Internal Server Error");
    assert_eq!(reply.read_calls, 0);
    assert_eq!(reply.custody_calls, 0);
    assert_eq!(reply.focus_calls, 0);
}

/// The snapshot read faulting is the caller's safe 500, and its cause never leaks.
#[tokio::test]
async fn a_snapshot_read_failure_is_a_safe_500() {
    let reply = get(
        ReadOutcome::Fails,
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Present(5),
        FocusOutcome::Focused(conversation("c-open")),
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.body, "Internal Server Error");
    assert!(!reply.body.contains("snapshot gone"));
    assert_eq!(reply.custody_calls, 0);
}

/// A custody read fault after the gate is the caller's safe 500.
#[tokio::test]
async fn a_custody_read_failure_is_a_safe_500() {
    let reply = get(
        ReadOutcome::Present(admitting_facts()),
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Fails,
        FocusOutcome::Focused(conversation("c-open")),
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.body, "Internal Server Error");
    assert!(!reply.body.contains("custody read gone"));
    assert_eq!(reply.focus_calls, 0);
}

/// A focus read fault after the generation match is the caller's safe 500.
#[tokio::test]
async fn a_focus_read_failure_is_a_safe_500() {
    let reply = get(
        ReadOutcome::Present(admitting_facts()),
        VisibilityOutcome::Readable(true),
        CustodyOutcome::Present(5),
        FocusOutcome::Fails,
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.body, "Internal Server Error");
    assert!(!reply.body.contains("focus read gone"));
}

/// The focus conversation's readability read faulting is the caller's safe 500 — a
/// redaction fault never silently becomes a visible or hidden focus.
#[tokio::test]
async fn a_focus_visibility_read_failure_is_a_safe_500() {
    let reply = get(
        ReadOutcome::Present(admitting_facts()),
        VisibilityOutcome::Fails,
        CustodyOutcome::Present(5),
        FocusOutcome::Focused(conversation("c-open")),
        true,
    )
    .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.body, "Internal Server Error");
    assert!(!reply.body.contains("visibility read gone"));
}
