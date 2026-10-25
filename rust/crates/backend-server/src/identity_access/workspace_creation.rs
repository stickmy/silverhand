//! `POST /api/workspaces` — create a workspace.
//!
//! A saga over reused authorities, in the reference order: admit the body, refuse a
//! taken name (409) or an unowned member agent (403) BEFORE any write, then perform
//! the durable steps — workspace + owner/user members, per-agent enrollment (the
//! SAME keyed enrollment authority the member-add doors use), and the Onboarding
//! channel + members + per-agent system messages — and finally hand the committed
//! per-agent onboarding targets to the aftermath (deliver awaited, start
//! best-effort). The `201` body reflects the committed owner pointer and the fresh
//! channel.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Serialize;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageId, agent_onboarding::WorkspaceOnboardingPeer, environment::SendEntropy,
    },
    foundation::identity::{AgentId, UserId},
    identity_access::{
        access::{ViewerWorkspacesChangedSink, WorkspaceRole},
        workspace_creation::{
            AcceptedMemberAgent, AgentOnboardingIntent, CreatedWorkspaceFace,
            FirstOnboardingOutcome, OnboardingChannelCommitted, WorkspaceCreationWrite,
            WorkspaceOnboardingTarget, WorkspaceProvisionClock, WorkspaceProvisionOutcome,
            WorkspaceProvisionRequest,
        },
        workspace_name::CanonicalWorkspaceName,
    },
};

use crate::foundation::http_error::{json_error as error, logged_500 as internal};
use crate::foundation::wire_admission::{ValidatedJson, ValidatedJsonRejection};
use crate::foundation::wire_patch::RequiredPatchField;

pub const WORKSPACES_PATH: &str = "/api/workspaces";

/// The per-agent onboarding post-commit effect, in the reference order and with the
/// reference criticality: `deliver` is AWAITED and CRITICAL — a failure returns
/// `Err`, which the route renders as `500` and STOPS the fan-out (this agent's
/// `start` and every later recipient are never touched, the durable residue stays).
/// `start` is fire-and-forget/best-effort — its failure is swallowed internally and
/// never gates the response.
pub trait WorkspaceCreationAftermath: Send + Sync + 'static {
    /// Deliver the onboarding message (critical, awaited). `Err` truncates the saga.
    fn deliver(
        &self,
        target: &WorkspaceOnboardingTarget,
        onboarding_message: &MessageId,
    ) -> impl std::future::Future<Output = Result<(), OnboardingDeliverFailed>> + Send;

    /// Start the agent's runtime — fire-and-forget. The implementation hands a
    /// detached leg to the owned effect executor and RETURNS IMMEDIATELY; a slow or
    /// never-resolving start can never gate the `201` or the next agent's
    /// append+deliver. Its failure is observable only inside the detached leg.
    fn start(&self, target: &WorkspaceOnboardingTarget);
}

/// The onboarding deliver failed — the reference's critical-leg fault that truncates
/// the fan-out and answers `500`.
#[derive(Debug)]
pub struct OnboardingDeliverFailed;

pub struct WorkspaceCreateState<W, B, C, A, S> {
    pub writer: W,
    pub entropy: B,
    pub clock: C,
    pub aftermath: A,
    pub membership_events: S,
}

struct Shared<W, B, C, A, S>(Arc<WorkspaceCreateState<W, B, C, A, S>>);
impl<W, B, C, A, S> Clone for Shared<W, B, C, A, S> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn workspace_create_router<W, B, C, A, S>(state: WorkspaceCreateState<W, B, C, A, S>) -> Router
where
    W: WorkspaceCreationWrite,
    B: SendEntropy + Sync + Send + 'static,
    C: WorkspaceProvisionClock + 'static,
    A: WorkspaceCreationAftermath,
    S: ViewerWorkspacesChangedSink,
{
    Router::new()
        .route(WORKSPACES_PATH, post(create::<W, B, C, A, S>))
        .with_state(Shared(Arc::new(state)))
}

/// `CreateWorkspaceInputSchema`: `{ name, description?, member_agent_ids=[],
/// member_user_ids=[] }`. `name` is the canonical workspace name (≤64 code points,
/// NFC, addressable); the two id arrays default to empty.
#[derive(serde::Deserialize, garde::Validate)]
struct CreateBody {
    #[garde(skip)]
    name: String,
    #[garde(skip)]
    #[serde(default)]
    description: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    member_agent_ids: Vec<String>,
    #[garde(skip)]
    #[serde(default)]
    member_user_ids: Vec<String>,
}

async fn create<W, B, C, A, S>(
    State(state): State<Shared<W, B, C, A, S>>,
    principal: Option<Extension<UserId>>,
    body: Result<ValidatedJson<CreateBody>, ValidatedJsonRejection>,
) -> Response
where
    W: WorkspaceCreationWrite,
    B: SendEntropy + Sync + Send + 'static,
    C: WorkspaceProvisionClock + 'static,
    A: WorkspaceCreationAftermath,
    S: ViewerWorkspacesChangedSink,
{
    let Some(Extension(viewer)) = principal else {
        return registration_required();
    };
    let ValidatedJson(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    // The canonical workspace name (≤64 code points, NFC, addressable) is a
    // semantic validation: a malformed name is the minimal `422 invalid input`.
    let Ok(name) = CanonicalWorkspaceName::parse(&body.name) else {
        return invalid_input();
    };
    let description = body.description.into_option();

    let state = &state.0;

    // Parse the requested member-agent ids: drop only the MALFORMED (a syntactically
    // invalid id is no id at all); existence + ownership are replayed UNDER the write
    // lock by the writer (#28/#41), so no preflight read races the durable setup here.
    // TS array multiplicity is preserved (#10): a repeated id stays repeated for the
    // per-item onboarding fan-out.
    let requested_agents: Vec<AgentId> = body
        .member_agent_ids
        .iter()
        .filter_map(|raw| AgentId::parse(raw.clone()).ok())
        .collect();

    // Member users: unknown ids silently dropped (checked in the writer via the
    // `users` FK-existence SELECT); here we keep only well-formed, deduped, non-owner.
    let mut member_user_ids: Vec<UserId> = Vec::new();
    for raw in &body.member_user_ids {
        if let Ok(user_id) = UserId::parse(raw.clone())
            && !member_user_ids.contains(&user_id)
            && user_id != viewer
        {
            member_user_ids.push(user_id);
        }
    }

    // ── The WHOLE synchronous durable-setup phase in ONE non-interleavable authority
    // boundary: name verdict → per-agent existence+ownership
    // replay (capturing each accepted agent's committed handle/runtime post-image) →
    // workspace+owner+users → per-agent membership+caps → onboarding channel+roster.
    // The writer MINTS the ids and SAMPLES the clock ITSELF, only after its name +
    // agent-owner gates pass, so a failing entropy/clock can never pre-empt a product
    // refusal. The FIRST accepted agent's onboarding message is ALSO written in that
    // same tx under a savepoint; only the SUBSEQUENT agents' messages
    // are the route-interleaved append→deliver step below (deliver failure truncates the
    // fan-out). ──
    let request = WorkspaceProvisionRequest {
        name: &name,
        description: description.as_deref(),
        owner_user: viewer.clone(),
        member_user_ids: &member_user_ids,
        requested_agents: &requested_agents,
    };
    let (face, committed_channel, member_agents, first_onboarding, member_users) = match state
        .writer
        .provision_workspace(&request, &state.entropy, &state.clock)
        .await
    {
        Ok(WorkspaceProvisionOutcome::Provisioned(provisioned)) => (
            provisioned.face,
            provisioned.onboarding_channel,
            provisioned.member_agents,
            provisioned.first_onboarding,
            provisioned.member_users,
        ),
        Ok(WorkspaceProvisionOutcome::NameTaken) => {
            return error(StatusCode::CONFLICT, "workspace name already exists");
        }
        Ok(WorkspaceProvisionOutcome::AgentNotOwned { agent_ids }) => {
            return agent_not_owned(&agent_ids);
        }
        Err(cause) => return internal(cause, "workspace-create durable provisioning failed"),
    };

    // Membership is already durable even if subsequent agent onboarding fails.
    // Notify only the committed audience, never the unvalidated request IDs.
    for user in &member_users {
        let role = if user == &viewer {
            WorkspaceRole::Owner
        } else {
            WorkspaceRole::Member
        };
        if let Err(error) = state
            .membership_events
            .viewer_added_to_workspace(user, &face.id, role)
            .await
        {
            tracing::warn!(
                error = crate::foundation::safe_log_field::safe_display(&error),
                workspace_id = face.id.as_str(),
                "workspace-create membership announcement failed"
            );
        }
    }

    let onboarding_channel_id = committed_channel.channel.clone();

    // The FIRST accepted agent's onboarding message is decided
    // inside `provision_workspace` under a savepoint. `Committed` ⇒ it is already
    // durable (atomic with setup), so its first observable concurrency point is the
    // deliver here. `Failed` ⇒ the savepoint rolled back (no message/head half-write)
    // but the setup residue committed; we return 500 with ZERO deliver/start effect.
    // `NoAgents` ⇒ nothing to seed. ──
    match &first_onboarding {
        FirstOnboardingOutcome::Failed => {
            tracing::error!(
                conversation_id = onboarding_channel_id.as_str(),
                "workspace-create first onboarding message failed — setup residue kept, zero effect"
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
        FirstOnboardingOutcome::Committed(first) => {
            let target = WorkspaceOnboardingTarget {
                conversation: onboarding_channel_id.clone(),
                agent: first.agent.clone(),
            };
            if state
                .aftermath
                .deliver(&target, &first.message_id)
                .await
                .is_err()
            {
                tracing::error!(
                    conversation_id = target.conversation.as_str(),
                    agent_id = target.agent.as_str(),
                    "workspace-create onboarding deliver failed — truncating fan-out"
                );
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
                    .into_response();
            }
            state.aftermath.start(&target);
        }
        FirstOnboardingOutcome::NoAgents => {}
    }

    // ── Per SUBSEQUENT agent (`member_agents[1..]`), in the reference order: build the
    // ID-LESS intent (recipient handle + peers only — the route does ZERO entropy, the
    // append writer mints the id and samples the clock itself in-lock),
    // append it canonically, AWAIT deliver (critical — a failure is a `500` that
    // truncates the fan-out: this agent's start and every later agent's
    // append+deliver+start never happen, only the channel + membership + the first
    // agent's message + prior agents' messages survive), then fire start (best-effort,
    // non-gating). Each recipient's handle is the CLOSED typed ParticipantHandle the
    // writer already parsed fail-closed IN the provisioning lock. ──
    for member in member_agents.iter().skip(1) {
        let intent = AgentOnboardingIntent {
            agent: member.agent.clone(),
            conversation: onboarding_channel_id.clone(),
            workspace_name: face.name.clone(),
            recipient_handle: member.handle.clone(),
            others: others_as_peers(&member_agents, &member.agent),
        };
        let committed_message = match state
            .writer
            .append_agent_onboarding_message(&intent, &state.entropy, &state.clock)
            .await
        {
            Ok(committed) => committed,
            Err(cause) => {
                return internal(cause, "workspace-create onboarding message append failed");
            }
        };
        let target = WorkspaceOnboardingTarget {
            conversation: onboarding_channel_id.clone(),
            agent: member.agent.clone(),
        };
        // Deliver is critical + awaited: a failure truncates the fan-out with a 500.
        if state
            .aftermath
            .deliver(&target, &committed_message.message_id)
            .await
            .is_err()
        {
            tracing::error!(
                conversation_id = target.conversation.as_str(),
                agent_id = target.agent.as_str(),
                "workspace-create onboarding deliver failed — truncating fan-out"
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
        // Start is fire-and-forget: it hands a detached leg to the owned executor and
        // returns immediately, so a slow/never-resolving start never gates the 201 or
        // the next agent.
        state.aftermath.start(&target);
    }

    (
        StatusCode::CREATED,
        Json(created_body(&face, &committed_channel)),
    )
        .into_response()
}

/// Project the OTHER member agents as onboarding peers (handle + runtime), for the
/// recipient's body roster.
fn others_as_peers(
    members: &[AcceptedMemberAgent],
    recipient: &AgentId,
) -> Vec<WorkspaceOnboardingPeer> {
    // Every member's handle is already the CLOSED typed ParticipantHandle the writer
    // parsed fail-closed in-lock — no filter_map that could silently drop a peer.
    members
        .iter()
        .filter(|member| &member.agent != recipient)
        .map(|member| WorkspaceOnboardingPeer {
            handle: member.handle.clone(),
            runtime: member.runtime.clone(),
        })
        .collect()
}

fn created_body(
    face: &CreatedWorkspaceFace,
    committed: &OnboardingChannelCommitted,
) -> CreateWorkspaceResponse {
    CreateWorkspaceResponse {
        workspace: ViewerWorkspaceDto {
            id: face.id.as_str().to_owned(),
            name: face.name.clone(),
            description: face.description.clone(),
            owner_agent_id: None,
            owner_user_id: face.owner_user_id.as_ref().map(|id| id.as_str().to_owned()),
            dm_channel_id: None,
            dm_channel_ids: Vec::new(),
            archived_at: None,
            created_at: face.created_at,
            updated_at: face.updated_at,
        },
        onboarding_channel_id: committed.channel.as_str().to_owned(),
        channel: ChannelConversationDto {
            id: committed.channel.as_str().to_owned(),
            workspace_id: Some(committed.workspace.as_str().to_owned()),
            placement_kind: "workspace",
            roster_policy: "open",
            title: committed.title.clone(),
            status: committed.status.clone(),
            spawned_by_agent_id: None,
            closed_at: None,
            close_summary: None,
            archived_at: None,
            created_at: committed.created_at,
            updated_at: committed.updated_at,
            visibility: committed.visibility.clone(),
        },
    }
}

/// `{ workspace, onboarding_channel_id, channel }`.
#[derive(Serialize)]
struct CreateWorkspaceResponse {
    workspace: ViewerWorkspaceDto,
    onboarding_channel_id: String,
    channel: ChannelConversationDto,
}

/// `ViewerWorkspaceDtoSchema`.
#[derive(Serialize)]
struct ViewerWorkspaceDto {
    id: String,
    name: String,
    description: Option<String>,
    owner_agent_id: Option<String>,
    owner_user_id: Option<String>,
    dm_channel_id: Option<String>,
    dm_channel_ids: Vec<String>,
    archived_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

/// `ChannelConversationDtoSchema`.
#[derive(Serialize)]
struct ChannelConversationDto {
    id: String,
    workspace_id: Option<String>,
    placement_kind: &'static str,
    roster_policy: &'static str,
    title: String,
    status: String,
    spawned_by_agent_id: Option<String>,
    closed_at: Option<i64>,
    close_summary: Option<String>,
    archived_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
    visibility: String,
}

fn invalid_input() -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({ "error": "invalid input" })),
    )
        .into_response()
}

fn agent_not_owned(agent_ids: &[AgentId]) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "you can only add agents you created — someone else's can be brought in only by its creator",
            "code": "agent_not_owned",
            "agent_ids": agent_ids.iter().map(AgentId::as_str).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

fn registration_required() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "viewer has not authenticated yet",
            "code": "registration_required"
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::extract::Extension;
    use axum::http::Request;
    use silverhand_backend_sqlite::conversation_messaging::messaging::HostEnvironment;
    use silverhand_backend_sqlite::identity_access::workspace_creation::SqliteWorkspaceCreation;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1','owner','Owner',1,1); \
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1','Host','local',1); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at, session_scope) \
  VALUES ('agent-a','alice','Alice','claude','host-1','user-1',1,1,'conversation'); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at, session_scope) \
  VALUES ('agent-b','bob','Bob','codex','host-1','user-1',1,1,'conversation');";

    #[derive(Clone, Copy)]
    struct FixedClock;
    impl WorkspaceProvisionClock for FixedClock {
        fn now_ms(&self) -> i64 {
            1000
        }
    }

    /// A clock whose every `now_ms()` returns a DISTINCT, increasing instant — used to
    /// prove the onboarding messages sample the clock PER APPEND,
    /// not once at provisioning time. The first call is the provisioning boundary
    /// sample; subsequent calls are the per-message appends.
    #[derive(Clone)]
    struct MonotonicClock(std::sync::Arc<std::sync::atomic::AtomicI64>);
    impl WorkspaceProvisionClock for MonotonicClock {
        fn now_ms(&self) -> i64 {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        }
    }

    // A controllable aftermath: records deliver+start calls in order; the first
    // deliver fails.
    #[derive(Clone)]
    struct FailingFirstDeliver {
        events: std::sync::Arc<Mutex<Vec<String>>>,
        deliver_calls: std::sync::Arc<AtomicUsize>,
    }
    impl WorkspaceCreationAftermath for FailingFirstDeliver {
        async fn deliver(
            &self,
            target: &WorkspaceOnboardingTarget,
            _: &MessageId,
        ) -> Result<(), OnboardingDeliverFailed> {
            let n = self.deliver_calls.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap()
                .push(format!("deliver:{}", target.agent.as_str()));
            if n == 0 {
                Err(OnboardingDeliverFailed)
            } else {
                Ok(())
            }
        }
        fn start(&self, target: &WorkspaceOnboardingTarget) {
            self.events
                .lock()
                .unwrap()
                .push(format!("start:{}", target.agent.as_str()));
        }
    }

    async fn seeded_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use sqlx::{Connection, Executor, SqliteConnection};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wc-route.sqlite");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        (dir, pool)
    }

    #[derive(Clone, Default)]
    struct RecordedMemberships(std::sync::Arc<Mutex<Vec<(UserId, WorkspaceRole)>>>);

    impl ViewerWorkspacesChangedSink for RecordedMemberships {
        async fn viewer_added_to_workspace(
            &self,
            user: &UserId,
            _: &silverhand_backend_core::foundation::identity::WorkspaceId,
            role: WorkspaceRole,
        ) -> Result<(), silverhand_backend_core::identity_access::access::ViewerAnnounceFailure>
        {
            self.0.lock().unwrap().push((user.clone(), role));
            Ok(())
        }
    }

    #[tokio::test]
    async fn first_deliver_failure_truncates_the_fanout_before_the_second_agents_message() {
        let (_dir, pool) = seeded_pool().await;
        let events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let membership_events = RecordedMemberships::default();
        let router = workspace_create_router(WorkspaceCreateState {
            membership_events: membership_events.clone(),
            writer: SqliteWorkspaceCreation::from_pool(pool.clone()),
            entropy: HostEnvironment,
            clock: FixedClock,
            aftermath: FailingFirstDeliver {
                events: std::sync::Arc::clone(&events),
                deliver_calls: std::sync::Arc::new(AtomicUsize::new(0)),
            },
        })
        .layer(Extension(UserId::parse("user-1").unwrap()));

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/workspaces")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"Team","member_agent_ids":["agent-a","agent-b"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Deliver failed → 500, fan-out truncated.
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        assert_eq!(
            *membership_events.0.lock().unwrap(),
            vec![(UserId::parse("user-1").unwrap(), WorkspaceRole::Owner)],
            "committed membership must be announced even when onboarding returns 500"
        );

        // Exactly ONE deliver attempt (the first agent), and NO start (the first
        // agent's start never runs, the second agent is never reached).
        let recorded = events.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "only the first deliver ran: {recorded:?}"
        );
        assert!(recorded[0].starts_with("deliver:"));

        // Durable residue: the workspace, its onboarding channel, and EXACTLY ONE
        // onboarding message (the first agent's) survive; the second agent's message
        // row was never written.
        let workspaces: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(workspaces, 1, "workspace committed before the fan-out");
        let channels: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE title='Onboarding'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(channels, 1, "onboarding channel committed");
        let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            messages, 1,
            "only the first agent's onboarding message was appended before the deliver failed"
        );
    }

    #[tokio::test]
    async fn a_successful_two_agent_create_delivers_then_starts_each_agent_in_order() {
        let (_dir, pool) = seeded_pool().await;
        let events = std::sync::Arc::new(Mutex::new(Vec::new()));
        // A never-failing aftermath.
        #[derive(Clone)]
        struct Recording(std::sync::Arc<Mutex<Vec<String>>>);
        impl WorkspaceCreationAftermath for Recording {
            async fn deliver(
                &self,
                target: &WorkspaceOnboardingTarget,
                _: &MessageId,
            ) -> Result<(), OnboardingDeliverFailed> {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("deliver:{}", target.agent.as_str()));
                Ok(())
            }
            fn start(&self, target: &WorkspaceOnboardingTarget) {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("start:{}", target.agent.as_str()));
            }
        }
        let router = workspace_create_router(WorkspaceCreateState {
            membership_events: RecordedMemberships::default(),
            writer: SqliteWorkspaceCreation::from_pool(pool.clone()),
            entropy: HostEnvironment,
            clock: FixedClock,
            aftermath: Recording(std::sync::Arc::clone(&events)),
        })
        .layer(Extension(UserId::parse("user-1").unwrap()));

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/workspaces")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"Team","member_agent_ids":["agent-a","agent-b"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        // #3: the 201 body's channel is PRIVATE (from the committed row), never a
        // hard-coded "public".
        let dto: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto["channel"]["visibility"], "private");
        assert_eq!(dto["channel"]["title"], "Onboarding");

        // Per agent, in order: deliver THEN start, agent-a fully before agent-b.
        let recorded = events.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![
                "deliver:agent-a".to_owned(),
                "start:agent-a".to_owned(),
                "deliver:agent-b".to_owned(),
                "start:agent-b".to_owned(),
            ]
        );
        // Both onboarding messages committed at monotone seq.
        let seqs: Vec<i64> = sqlx::query_scalar("SELECT seq FROM messages ORDER BY seq")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[tokio::test]
    async fn each_onboarding_message_samples_its_own_append_instant_not_the_provisioning_one() {
        // The per-agent onboarding messages sample the clock at their
        // OWN append (TS calls messages.insertForConversation per round). With a
        // monotonic clock, the two messages must carry DISTINCT created_at — NOT both
        // equal to the workspace-provisioning instant.
        let (_dir, pool) = seeded_pool().await;
        #[derive(Clone)]
        struct NoopAftermath;
        impl WorkspaceCreationAftermath for NoopAftermath {
            async fn deliver(
                &self,
                _t: &WorkspaceOnboardingTarget,
                _m: &MessageId,
            ) -> Result<(), OnboardingDeliverFailed> {
                Ok(())
            }
            fn start(&self, _t: &WorkspaceOnboardingTarget) {}
        }
        let clock = MonotonicClock(std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1000)));
        let router = workspace_create_router(WorkspaceCreateState {
            membership_events: RecordedMemberships::default(),
            writer: SqliteWorkspaceCreation::from_pool(pool.clone()),
            entropy: HostEnvironment,
            clock: clock.clone(),
            aftermath: NoopAftermath,
        })
        .layer(Extension(UserId::parse("user-1").unwrap()));
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/workspaces")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"Team","member_agent_ids":["agent-a","agent-b"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        // Two messages, DISTINCT per-append created_at (the monotonic clock advanced
        // between appends); neither equals the workspace-provisioning instant (1000).
        let created: Vec<i64> = sqlx::query_scalar("SELECT created_at FROM messages ORDER BY seq")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(created.len(), 2);
        assert_ne!(
            created[0], created[1],
            "each onboarding message samples its own append instant"
        );
        assert!(
            created[0] > 1000 && created[1] > 1000,
            "message instants are AFTER the provisioning boundary sample, not equal to it"
        );
    }
}
