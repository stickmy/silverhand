//! Authenticated cold face for `GET /api/agents/{id}`: one agent's profile row at the
//! tier [`agent_read_tier`] grants the caller.
//!
//! Viewers in the visibility ACL read the full DTO (`view: "privileged"`); any other
//! signed-in human reads [`AgentPublicDto`] (`view: "public"`) for an active agent.
//! An absent row, and a pending or failed row outside the ACL, are one
//! `404 {"error":"agent not found"}`.
//!
//! The resolved row is bound to the requested id before the tier is decided: a bundle
//! decoded for another agent is a resolver defect and takes the lane's safe `500`.
//!
//! `?include_failed=1` and `?include_pending=1` widen the privileged tier's stages
//! only; they never reach the public tier.
//!
//! The human principal's absence is a composition fault: fail-closed `500`.

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    foundation::identity::{AgentId, UserId},
    identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailReadModel},
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::capability::{
        agent_view::VisibleStages,
        read_tier::{ReadTier, agent_read_tier},
    },
};

use crate::foundation::error;

use crate::identity_access::agent_wire::{AgentDto, AgentPublicDto, agent_dto};

/// The product route template, in axum's `{id}` capture syntax.
pub const AGENT_DETAIL_PATH: &str = "/api/agents/{id}";

/// The not-found refusal string, pinned to the TypeScript handler
/// (`c.json({ error: 'agent not found' }, 404)`) so a fixture drift shows here.
const AGENT_NOT_FOUND: &str = "agent not found";

/// The unmapped-error body: the product route registers no `onError`, so an
/// unexpected failure falls to the framework default — a `500` with a bare
/// `text/plain` `Internal Server Error`, never a JSON envelope.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

/// The one query token that opts a stage into the view. Mirrors the TypeScript
/// `c.req.query('…') === '1'`: only the exact string `1` widens, so `0`, `true`, and
/// an absent param all leave the stage hidden.
const STAGE_OPT_IN: &str = "1";

#[derive(Clone)]
struct DetailState<R> {
    cold_read: R,
}

pub fn router<R>(cold_read: R) -> Router
where
    R: AgentDetailReadModel + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(AGENT_DETAIL_PATH, get(agent_detail::<R>))
        .with_state(DetailState { cold_read })
}

async fn agent_detail<R>(
    Path(agent_id): Path<String>,
    Query(flags): Query<StageFlags>,
    viewer: Option<Extension<UserId>>,
    State(state): State<DetailState<R>>,
) -> Response
where
    R: AgentDetailReadModel,
{
    let Some(Extension(viewer)) = viewer else {
        tracing::error!("agent-detail route reached without its human principal fact");
        return internal_error();
    };
    // A malformed id names no agent — the same 404 an absent row gets, so the surface
    // never distinguishes "unparseable" from "not there".
    let Ok(agent) = AgentId::parse(agent_id) else {
        return not_found();
    };

    // The agent row, its host LEFT JOIN, and its three ACL arms on ONE snapshot, read
    // BEFORE the gate.
    let detail = match state.cold_read.read_agent_detail(&agent).await {
        Ok(detail) => detail,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-detail read failed"
            );
            return internal_error();
        }
    };

    // Bind the resolved row to the request before consulting the ACL: a bundle whose
    // decoded id is not the one asked for is a resolver defect — the gate would judge
    // one row and the wire render another — so it takes the lane's safe 500. A short
    // read (no row) has nothing to bind; it is the ACL's absence arm.
    if let Some(detail) = detail.as_ref()
        && detail.source() != &agent
    {
        tracing::error!(
            requested = agent.as_str(),
            resolved = detail.source().as_str(),
            "agent-detail resolved for the wrong agent"
        );
        return internal_error();
    }

    // The flags widen the visible STAGES; the ACL is unaffected, so a cross-caller
    // failed or pending row still 404s with both set.
    let mut stages = vec![MaterializationStatus::Active];
    if opted_in(flags.include_failed.as_deref()) {
        stages.push(MaterializationStatus::Failed);
    }
    if opted_in(flags.include_pending.as_deref()) {
        stages.push(MaterializationStatus::PendingMaterialization);
    }
    let stages = VisibleStages::including(stages);

    // Absence, and a non-active row outside the visibility ACL, are one 404.
    let tier = agent_read_tier(
        &viewer,
        &agent,
        detail.as_ref().map(AgentDetailFacts::visibility),
        &stages,
    );
    let (Some(tier), Some(detail)) = (tier, detail) else {
        return not_found();
    };

    let agent = agent_dto(&detail);
    match tier {
        ReadTier::Privileged => Json(AgentDetailResponse {
            view: tier.as_str(),
            agent,
        })
        .into_response(),
        ReadTier::Public => Json(AgentPublicDetailResponse {
            view: tier.as_str(),
            agent: agent.into(),
        })
        .into_response(),
    }
}

/// The two stage-widening flags, read the framework-native way.
///
/// Each is the raw token of a SINGLE-valued query parameter. The frontend never
/// sends a repeated key, so which occurrence would win is deliberately NOT
/// contracted: `serde_urlencoded` rejects a duplicate key as a `400`, a loud
/// failure for what could only be a frontend bug — never a silent last-wins. A
/// stage opts in only on the exact token `1` (see [`opted_in`]).
#[derive(Deserialize)]
struct StageFlags {
    include_failed: Option<String>,
    include_pending: Option<String>,
}

/// Whether a flag opts its stage in. A stage widens only on the exact token `1` —
/// the same `c.req.query('…') === '1'` the TypeScript handler applies — so `0`,
/// `true`, an empty value, and an absent parameter all leave the stage hidden.
fn opted_in(value: Option<&str>) -> bool {
    value == Some(STAGE_OPT_IN)
}

/// Success body for a viewer in the visibility ACL: `{ "view": "privileged", "agent" }`.
#[derive(Serialize)]
struct AgentDetailResponse {
    view: &'static str,
    agent: AgentDto,
}

/// Success body for any other signed-in human: `{ "view": "public", "agent" }`.
#[derive(Serialize)]
struct AgentPublicDetailResponse {
    view: &'static str,
    agent: AgentPublicDto,
}

/// The flat refusal body, mirroring the product `{ "error": ... }`.
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: AGENT_NOT_FOUND,
        }),
    )
        .into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

#[cfg(test)]
mod tests {
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
        foundation::identity::{AgentId, UserId},
        foundation::persistence::PersistenceFailure,
        identity_access::agent_detail_read::{
            AgentDetailFacts, AgentDetailProjection, AgentDetailReadModel,
        },
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::runtime::RuntimeKind,
    };
    use tower::ServiceExt;

    use super::router;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    /// Which ACL arm should admit `viewer-1`, plus one that admits only OTHER people.
    enum Arm {
        Creator,
        DirectPair,
        Workspace,
        NobodyRelevant,
    }

    /// `(creator, direct_pair_human_endpoints, shared_workspace_members)` for the arm.
    /// The creator is always some human — the row's `created_by_user_id` — so an arm
    /// that tests a non-creator path gives the creator a DISTINCT id, which lets a
    /// test tell the row's creator apart from whoever was admitted.
    fn arms(arm: Arm) -> (UserId, Vec<UserId>, Vec<UserId>) {
        match arm {
            Arm::Creator => (user("viewer-1"), vec![], vec![]),
            Arm::DirectPair => (user("creator-user"), vec![user("viewer-1")], vec![]),
            Arm::Workspace => (user("creator-user"), vec![], vec![user("viewer-1")]),
            Arm::NobodyRelevant => (
                user("someone-else"),
                vec![user("pair-peer")],
                vec![user("member")],
            ),
        }
    }

    /// A fully-populated projection for agent `a1`, so a full-DTO parity assertion
    /// exercises every wire column and its derivations.
    fn full_projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_string(),
            display_name: "Agent One".to_string(),
            runtime: RuntimeKind::Qoder,
            model: Some("sonnet".to_string()),
            reasoning_effort: Some("high".to_string()),
            fast_mode: Some(true),
            host_id: "h1".to_string(),
            host_display_label: Some("Host One Label".to_string()),
            host_name: Some("host-one".to_string()),
            description: Some("a description".to_string()),
            session_scope: "agent".to_string(),
            avatar_storage_key: Some("avatars/a1.png".to_string()),
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    fn facts_with(
        source: &str,
        arm: Arm,
        status: MaterializationStatus,
        projection: AgentDetailProjection,
    ) -> AgentDetailFacts {
        let (creator, direct_pair, workspace) = arms(arm);
        AgentDetailFacts::from_source(
            agent(source),
            creator,
            direct_pair,
            workspace,
            status,
            projection,
        )
    }

    /// The ordinary admitting bundle: agent `a1`, creator arm, active stage.
    fn admitting() -> AgentDetailFacts {
        facts_with(
            "a1",
            Arm::Creator,
            MaterializationStatus::Active,
            full_projection(),
        )
    }

    #[derive(Clone)]
    enum ViewOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
    }

    fn present(facts: AgentDetailFacts) -> ViewOutcome {
        ViewOutcome::Present(Box::new(facts))
    }

    #[derive(Clone)]
    struct FakeDetail {
        view: ViewOutcome,
        calls: Arc<AtomicUsize>,
    }

    impl AgentDetailReadModel for FakeDetail {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            self.calls.fetch_add(1, SeqCst);
            match &self.view {
                ViewOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                ViewOutcome::Absent => Ok(None),
                ViewOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                    "detail gone",
                ))),
            }
        }
    }

    struct Reply {
        status: StatusCode,
        body: String,
        calls: usize,
    }

    async fn get(view: ViewOutcome, query: &str, with_principal: bool) -> Reply {
        get_for(view, "a1", query, with_principal).await
    }

    /// Drive the route for `/api/agents/{path_id}{query}`; `path_id` lets a test ask
    /// about a different id than the resolver returns (the source-bind case).
    async fn get_for(view: ViewOutcome, path_id: &str, query: &str, with_principal: bool) -> Reply {
        let calls = Arc::new(AtomicUsize::new(0));
        let reads = FakeDetail {
            view,
            calls: calls.clone(),
        };
        let uri = format!("/api/agents/{path_id}{query}");
        let mut request = Request::get(&uri);
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        let response = router(reads)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: String::from_utf8(body.to_vec()).unwrap(),
            calls: calls.load(SeqCst),
        }
    }

    fn parsed(body: &str) -> Value {
        serde_json::from_str::<Value>(body).unwrap()
    }

    #[tokio::test]
    async fn each_visibility_arm_admits_the_viewer_on_its_own() {
        for arm in [Arm::Creator, Arm::DirectPair, Arm::Workspace] {
            let reply = get(
                present(facts_with(
                    "a1",
                    arm,
                    MaterializationStatus::Active,
                    full_projection(),
                )),
                "",
                true,
            )
            .await;
            assert_eq!(reply.status, StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn a_viewer_in_no_arm_reads_exactly_the_public_tier() {
        let reply = get(
            present(facts_with(
                "a1",
                Arm::NobodyRelevant,
                MaterializationStatus::Active,
                full_projection(),
            )),
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "view": "public",
                "agent": {
                    "id": "a1",
                    "name": "agent-one",
                    "display_name": "Agent One",
                    "runtime": "qoder",
                    "model": "sonnet",
                    "reasoning_effort": "high",
                    "fast_mode": true,
                    "host_id": "h1",
                    "host_label": "Host One Label",
                    "description": "a description",
                    "created_by_user_id": "someone-else",
                    "avatar_url": "/api/agents/a1/avatar?v=2000",
                    "created_at": 1000,
                }
            })
        );
    }

    #[tokio::test]
    async fn an_admitted_viewer_reads_the_privileged_tier() {
        let reply = get(present(admitting()), "", true).await;
        assert_eq!(reply.status, StatusCode::OK);
        let body = parsed(&reply.body);
        assert_eq!(body["view"], "privileged");
        assert_eq!(body["agent"]["session_scope"], "agent");
    }

    #[tokio::test]
    async fn a_hidden_stage_is_a_404_to_a_viewer_in_no_arm() {
        for status in [
            MaterializationStatus::PendingMaterialization,
            MaterializationStatus::Failed,
        ] {
            let reply = get(
                present(facts_with(
                    "a1",
                    Arm::NobodyRelevant,
                    status,
                    full_projection(),
                )),
                "",
                true,
            )
            .await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND);
            assert_eq!(parsed(&reply.body), json!({ "error": "agent not found" }));
        }
    }

    #[tokio::test]
    async fn an_absent_agent_is_the_same_404() {
        let reply = get(ViewOutcome::Absent, "", true).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(parsed(&reply.body), json!({ "error": "agent not found" }));
        assert_eq!(reply.calls, 1);
    }

    #[tokio::test]
    async fn a_hidden_stage_is_the_same_404_under_the_default_view() {
        for status in [
            MaterializationStatus::PendingMaterialization,
            MaterializationStatus::Failed,
        ] {
            let reply = get(
                present(facts_with("a1", Arm::Creator, status, full_projection())),
                "",
                true,
            )
            .await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND);
            assert_eq!(parsed(&reply.body), json!({ "error": "agent not found" }));
        }
    }

    #[tokio::test]
    async fn include_failed_reveals_a_failed_row_to_an_admitted_caller() {
        let reply = get(
            present(facts_with(
                "a1",
                Arm::Creator,
                MaterializationStatus::Failed,
                full_projection(),
            )),
            "?include_failed=1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            parsed(&reply.body)["agent"]["materialization_status"],
            "failed"
        );
    }

    #[tokio::test]
    async fn include_pending_reveals_a_pending_row_to_an_admitted_caller() {
        let reply = get(
            present(facts_with(
                "a1",
                Arm::Creator,
                MaterializationStatus::PendingMaterialization,
                full_projection(),
            )),
            "?include_pending=1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            parsed(&reply.body)["agent"]["materialization_status"],
            "pending_materialization"
        );
    }

    #[tokio::test]
    async fn a_non_one_flag_value_does_not_widen_the_stage() {
        // Only the exact string `1` opts a stage in; `0` leaves failed hidden.
        for query in [
            "?include_failed=0",
            "?include_failed=true",
            "?include_failed=",
        ] {
            let reply = get(
                present(facts_with(
                    "a1",
                    Arm::Creator,
                    MaterializationStatus::Failed,
                    full_projection(),
                )),
                query,
                true,
            )
            .await;
            assert_eq!(
                reply.status,
                StatusCode::NOT_FOUND,
                "query {query} must not widen"
            );
        }
    }

    #[tokio::test]
    async fn the_stage_widener_never_reaches_the_public_tier() {
        // A caller in no arm stays a 404 for a failed row even with BOTH flags set.
        let reply = get(
            present(facts_with(
                "a1",
                Arm::NobodyRelevant,
                MaterializationStatus::Failed,
                full_projection(),
            )),
            "?include_failed=1&include_pending=1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(parsed(&reply.body), json!({ "error": "agent not found" }));
    }

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_reads() {
        let reply = get(present(admitting()), "", false).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.calls, 0);
    }

    #[tokio::test]
    async fn a_detail_read_failure_is_a_safe_500_that_leaks_no_cause() {
        let reply = get(ViewOutcome::Fails, "", true).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
    }

    #[tokio::test]
    async fn a_bundle_resolved_for_another_agent_is_a_safe_500() {
        // The URL asks about `a1`, but the resolver returns a bundle whose decoded id
        // is `a2`: the gate would judge one row and the wire render another. The
        // source-bind refuses it as a composition defect, even though the creator arm
        // would admit the caller.
        let spliced = facts_with(
            "a2",
            Arm::Creator,
            MaterializationStatus::Active,
            full_projection(),
        );
        let reply = get_for(present(spliced), "a1", "", true).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
    }

    #[tokio::test]
    async fn the_null_columns_and_derivations_project_faithfully() {
        // Every optional column NULL, a whitespace-only host label, and an empty
        // avatar key: host_label falls through to the host name, avatar_url is null,
        // and each absent column is an explicit JSON null.
        let projection = AgentDetailProjection {
            name: "agent-one".to_string(),
            display_name: String::new(),
            runtime: RuntimeKind::Qoder,
            model: None,
            reasoning_effort: None,
            fast_mode: None,
            host_id: "h1".to_string(),
            host_display_label: Some("   ".to_string()),
            host_name: Some("host-one".to_string()),
            description: None,
            session_scope: "conversation".to_string(),
            avatar_storage_key: Some(String::new()),
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        };
        let reply = get(
            present(facts_with(
                "a1",
                Arm::Creator,
                MaterializationStatus::Active,
                projection,
            )),
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let agent = parsed(&reply.body);
        let agent = &agent["agent"];
        assert_eq!(agent["display_name"], "");
        assert_eq!(agent["host_label"], "host-one");
        for key in [
            "model",
            "reasoning_effort",
            "fast_mode",
            "avatar_url",
            "description",
            "materialization_failed_code",
            "materialization_failed_at",
            "deletion_started_at",
        ] {
            assert_eq!(
                agent.get(key),
                Some(&Value::Null),
                "{key} must be present as null"
            );
        }
    }

    #[tokio::test]
    async fn a_gone_host_leaves_the_label_null() {
        // The LEFT JOIN found no host row, so both label columns are absent and
        // host_label is null — the only case that is null rather than a fallback.
        let mut projection = full_projection();
        projection.host_display_label = None;
        projection.host_name = None;
        let reply = get(
            present(facts_with(
                "a1",
                Arm::Creator,
                MaterializationStatus::Active,
                projection,
            )),
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            parsed(&reply.body)["agent"].get("host_label"),
            Some(&Value::Null)
        );
    }

    #[tokio::test]
    async fn the_wire_creator_is_the_row_creator_not_the_admitted_viewer() {
        // `viewer-1` is admitted through the WORKSPACE arm, not as the creator (whose
        // id is `creator-user`). The wire still renders the row's creator, not the
        // viewer: the ACL creator arm and the wire `created_by_user_id` are one sealed
        // value, so no read can admit one human and render another.
        let reply = get(
            present(facts_with(
                "a1",
                Arm::Workspace,
                MaterializationStatus::Active,
                full_projection(),
            )),
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            parsed(&reply.body)["agent"]["created_by_user_id"],
            "creator-user"
        );
    }
}
