//! Authenticated cold face for `GET /api/agents`: every agent this caller may see.
//!
//! The single-agent faces name an id, so "you may not see it" has to be disguised as
//! "it does not exist". The list face names none: a hidden agent is simply ABSENT
//! from the array. There is no existence oracle to protect here and no 404 arm at
//! all — a caller admitted to nothing gets `{"agents":[]}`.
//!
//! The perimeter still supplies a HUMAN principal, so the two arms that only a
//! non-human caller can reach — the system arm, which also bypasses the stage
//! filter, and the agent-self arm — are unreachable at this route. An absent
//! principal fact is fail-closed: the lane's safe `500`, having read nothing.
//!
//! Order and filtering belong to the port, not to the router. This module parses
//! `include_failed`, `include_pending` and `scope` with the framework's typed
//! extractor and hands them down; it never re-sorts, and it never drops a row the
//! port returned.
//!
//! Projection is shared with detail through [`crate::identity_access::agent_wire`]: the two responses
//! differ ONLY in their wrapper, so one projector serves both and neither can drift
//! a field.

use axum::{
    Json, Router,
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    foundation::identity::UserId,
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agent_list_read::{AgentListReadModel, ListScope, visible_agents},
    identity_access::capability::agent_view::VisibleStages,
};

use crate::foundation::error;

use crate::identity_access::agent_wire::{AgentDto, agent_dto};

/// The product route template.
pub const AGENTS_PATH: &str = "/api/agents";

/// The one `?scope=` value that narrows to the caller's own agents. Mirrors the
/// TypeScript `c.req.query('scope') === 'owned'`: any other value is the default
/// every-visible-agent view.
const SCOPE_OWNED: &str = "owned";

/// The one query token that opts a stage into the view — the same
/// `c.req.query('…') === '1'` the single-agent faces apply.
const STAGE_OPT_IN: &str = "1";

/// The unmapped-error body: like detail, the product route registers no `onError`,
/// so an unexpected failure falls to the framework default — a `500` with a bare
/// `text/plain` body, never a JSON envelope.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

#[derive(Clone)]
struct ListState<R> {
    cold_read: R,
}

pub fn router<R>(cold_read: R) -> Router
where
    R: AgentListReadModel + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(AGENTS_PATH, get(agents_list::<R>))
        .with_state(ListState { cold_read })
}

async fn agents_list<R>(
    Query(query): Query<ListQuery>,
    viewer: Option<Extension<UserId>>,
    State(state): State<ListState<R>>,
) -> Response
where
    R: AgentListReadModel,
{
    // The perimeter's human principal is proof it ran. Its absence is a composition
    // fault, not a request, so it fails closed having read nothing.
    let Some(Extension(viewer)) = viewer else {
        tracing::error!("agent-list route reached without its human principal fact");
        return internal_error();
    };

    // Narrow to candidates first; the ACL decision is the core's, per row.
    let candidates = match state.cold_read.read_agent_candidates(&viewer).await {
        Ok(candidates) => candidates,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-list read failed"
            );
            return internal_error();
        }
    };

    // The flags widen the visible STAGES; the ACL is unaffected.
    let mut stages = vec![MaterializationStatus::Active];
    if opted_in(query.include_failed.as_deref()) {
        stages.push(MaterializationStatus::Failed);
    }
    if opted_in(query.include_pending.as_deref()) {
        stages.push(MaterializationStatus::PendingMaterialization);
    }
    let stages = VisibleStages::including(stages);

    let scope = if query.scope.as_deref() == Some(SCOPE_OWNED) {
        ListScope::Owned
    } else {
        ListScope::All
    };

    // The decider re-runs the ONE visibility authority per candidate. A candidate it
    // refuses on ACL grounds means the narrowing disagreed with the predicate — a
    // fact-source defect, not a product answer — so it fails closed rather than
    // dropping the row silently.
    let visible = match visible_agents(&viewer, &candidates, &stages, scope) {
        Ok(visible) => visible,
        Err(mismatch) => {
            tracing::error!(
                agent = mismatch.agent().as_str(),
                "agent-list candidate refused by the visibility authority"
            );
            return internal_error();
        }
    };

    let agents = visible.into_iter().map(agent_dto).collect::<Vec<_>>();
    Json(AgentListResponse { agents }).into_response()
}

/// The stage-widening and scope flags, read the framework-native way. Each is the raw
/// token of a SINGLE-valued query parameter; `serde_urlencoded` rejects a duplicate
/// key as a `400`, a loud failure for what could only be a frontend bug.
#[derive(Deserialize)]
struct ListQuery {
    include_failed: Option<String>,
    include_pending: Option<String>,
    scope: Option<String>,
}

/// Whether a flag opts its stage in — only on the exact token `1`, so `0`, `true`,
/// an empty value, and an absent parameter all leave the stage hidden.
fn opted_in(value: Option<&str>) -> bool {
    value == Some(STAGE_OPT_IN)
}

/// Success body: `{ "agents": [AgentDto] }`. The array carries the shared
/// [`crate::identity_access::agent_wire`] projector; the list and detail faces differ only here, in
/// the wrapper.
#[derive(Serialize)]
struct AgentListResponse {
    agents: Vec<AgentDto>,
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
    use serde_json::Value;
    use silverhand_backend_core::{
        foundation::identity::{AgentId, UserId},
        foundation::persistence::PersistenceFailure,
        identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailProjection},
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agent_list_read::AgentListReadModel,
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

    /// A minimal projection with every optional column absent.
    fn bare_projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent".to_string(),
            display_name: "Agent".to_string(),
            runtime: RuntimeKind::Qoder,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            host_id: "h1".to_string(),
            host_display_label: None,
            host_name: None,
            description: None,
            session_scope: "conversation".to_string(),
            avatar_storage_key: None,
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    /// `viewer-1` admitted through the workspace arm; a DISTINCT creator, so the wire
    /// `created_by_user_id` is the row's creator, not the admitted viewer.
    fn admitted(id: &str, status: MaterializationStatus) -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent(id),
            user("creator-user"),
            [],
            [user("viewer-1")],
            status,
            bare_projection(),
        )
    }

    #[derive(Clone)]
    enum ListOutcome {
        Candidates(Vec<AgentDetailFacts>),
        Fails,
    }

    #[derive(Clone)]
    struct FakeList {
        outcome: ListOutcome,
        calls: Arc<AtomicUsize>,
    }

    impl AgentListReadModel for FakeList {
        async fn read_agent_candidates(
            &self,
            _viewer: &UserId,
        ) -> Result<Vec<AgentDetailFacts>, PersistenceFailure> {
            self.calls.fetch_add(1, SeqCst);
            match &self.outcome {
                ListOutcome::Candidates(candidates) => Ok(candidates.clone()),
                ListOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                    "list gone",
                ))),
            }
        }
    }

    struct Reply {
        status: StatusCode,
        body: String,
        calls: usize,
    }

    async fn get(outcome: ListOutcome, query: &str, with_principal: bool) -> Reply {
        let calls = Arc::new(AtomicUsize::new(0));
        let reads = FakeList {
            outcome,
            calls: calls.clone(),
        };
        let uri = format!("/api/agents{query}");
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

    fn ids(body: &str) -> Vec<String> {
        serde_json::from_str::<Value>(body).unwrap()["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|agent| agent["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_hidden_stage_is_filtered_and_include_failed_widens() {
        let candidates = vec![
            admitted("a1", MaterializationStatus::Active),
            admitted("a2", MaterializationStatus::Failed),
        ];

        // Default view hides the failed row.
        let default = get(ListOutcome::Candidates(candidates.clone()), "", true).await;
        assert_eq!(default.status, StatusCode::OK);
        assert_eq!(ids(&default.body), ["a1"]);

        // Opting the failed stage in reveals it — the same candidates.
        let widened = get(
            ListOutcome::Candidates(candidates),
            "?include_failed=1",
            true,
        )
        .await;
        assert_eq!(widened.status, StatusCode::OK);
        assert_eq!(ids(&widened.body), ["a1", "a2"]);
    }

    #[tokio::test]
    async fn a_candidate_the_acl_refuses_is_a_fail_closed_500() {
        // A candidate that admits NOBODY but its distinct creator: the narrowing
        // offered a row the predicate rejects for `viewer-1`, so the facts disagree.
        let stranger = AgentDetailFacts::from_source(
            agent("a1"),
            user("someone-else"),
            [],
            [],
            MaterializationStatus::Active,
            bare_projection(),
        );
        let reply = get(ListOutcome::Candidates(vec![stranger]), "", true).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, super::INTERNAL_ERROR_BODY);
        assert_eq!(reply.calls, 1);
    }

    #[tokio::test]
    async fn a_missing_principal_is_a_500_reading_nothing() {
        let reply = get(ListOutcome::Candidates(vec![]), "", false).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, super::INTERNAL_ERROR_BODY);
        // Fail-closed before the read: the fact source is never touched.
        assert_eq!(reply.calls, 0);
    }

    #[tokio::test]
    async fn a_read_failure_is_a_500() {
        let reply = get(ListOutcome::Fails, "", true).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, super::INTERNAL_ERROR_BODY);
        assert_eq!(reply.calls, 1);
    }
}
