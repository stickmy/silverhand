//! `GET /api/agents/{id}/schedules` — the owner's schedule list.
//!
//! The query grammar (`status` in the closed family, integer `limit` in
//! `1..=200`) is parsed with a DEFERRED parse: the visibility/authority floor
//! ([`gate_agent_edit`]) runs FIRST, and only a caller who has already cleared it
//! can reach a query-validation 400. An eager `Query<T>` extractor would run
//! during extraction — before the floor — and turn an invalid query into an
//! existence oracle, so this face reads the raw query string and validates it by
//! hand after the gate.

use axum::{
    Json, Router,
    extract::{Extension, Path, RawQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use silverhand_backend_core::{
    conversation_messaging::messaging::schedule::{
        ScheduleListLimit, ScheduleListQuery, ScheduleListReadModel, ScheduleStatusFilter,
    },
    foundation::identity::UserId,
    identity_access::agent_detail_read::AgentDetailReadModel,
};

use super::{gate_agent_edit, wire::ScheduleDto};

/// The product route template, in axum's `{id}` capture syntax.
pub const SCHEDULES_PATH: &str = "/api/agents/{id}/schedules";

/// The route name for the shared gate's fail-closed log line.
const ROUTE: &str = "agent-schedules-list";

#[derive(Clone)]
struct ListState<D, R> {
    detail_reads: D,
    list_reads: R,
}

/// The gate read (visibility+authority) and the schedule list read are TWO
/// distinct authorities on TWO parameters — never fused. Each stays its own sole
/// read authority, exactly as the sibling write faces carry their detail read
/// plus their domain port.
pub fn router<D, R>(detail_reads: D, list_reads: R) -> Router
where
    D: AgentDetailReadModel + Clone + Send + Sync + 'static,
    R: ScheduleListReadModel + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(SCHEDULES_PATH, get(list_schedules::<D, R>))
        .with_state(ListState {
            detail_reads,
            list_reads,
        })
}

async fn list_schedules<D, R>(
    Path(agent_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    viewer: Option<Extension<UserId>>,
    State(state): State<ListState<D, R>>,
) -> Response
where
    D: AgentDetailReadModel,
    R: ScheduleListReadModel,
{
    // FLOOR FIRST: visibility + authority. Only past it does the query parse run.
    let (agent, _viewer) = match gate_agent_edit(&state.detail_reads, viewer, agent_id, ROUTE).await
    {
        Ok(bound) => bound,
        Err(refusal) => return refusal.into_response(),
    };

    // DEFERRED PARSE: the closed query grammar, validated after the floor.
    let raw = raw_query.as_deref().unwrap_or("");
    let filter = match query_param(raw, "status").as_deref() {
        None | Some("") => ScheduleStatusFilter::All,
        Some(token) => match ScheduleStatusFilter::parse(token) {
            Ok(filter) => filter,
            Err(_) => return invalid_query(),
        },
    };
    let limit = match query_param(raw, "limit").as_deref() {
        None | Some("") => ScheduleListLimit::DEFAULT,
        Some(raw_limit) => {
            let Ok(value) = raw_limit.trim().parse::<i64>() else {
                return invalid_query();
            };
            match ScheduleListLimit::parse(value) {
                Ok(limit) => limit,
                Err(_) => return invalid_query(),
            }
        }
    };

    let query = ScheduleListQuery {
        owner_agent_id: agent,
        statuses: filter.expand(),
        limit,
    };
    match state.list_reads.list_owned_schedules(&query).await {
        Ok(views) => {
            let schedules: Vec<ScheduleDto> = views.into_iter().map(ScheduleDto::from).collect();
            (StatusCode::OK, Json(ListBody { schedules })).into_response()
        }
        Err(cause) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&cause))
                ),
                "agent-schedules-list read failed"
            );
            super::internal_error()
        }
    }
}

/// First-occurrence value for `key`, decoded with the SAME form-urlencoded
/// grammar Hono's `c.req.query(key)` applies (percent-decoding, `+`→space). A
/// bare key with no `=` yields an empty value; `find` keeps the first pair, so a
/// repeated key takes its first occurrence, exactly as `c.req.query` does. The
/// closed status family and a decimal `limit` never need decoding, but a caller
/// may still encode them (`status=%61ctive`), so the decode is unconditional.
fn query_param(raw: &str, key: &str) -> Option<String> {
    form_urlencoded::parse(raw.as_bytes())
        .find(|(k, _)| k.as_ref() == key)
        .map(|(_, value)| value.into_owned())
}

/// The invalid-query 400. TypeScript also attaches `issues` (the Zod error
/// array); that is a validator-internal artifact, not a stable contract, so the
/// Rust face carries only the stable `{ "error": "invalid query" }`.
fn invalid_query() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(InvalidQueryBody {
            error: "invalid query",
        }),
    )
        .into_response()
}

/// Success body: `{ "schedules": [ ... ] }`.
#[derive(Serialize)]
struct ListBody {
    schedules: Vec<ScheduleDto>,
}

#[derive(Serialize)]
struct InvalidQueryBody {
    error: &'static str,
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        conversation_messaging::messaging::schedule::{ScheduleSpec, ScheduleStatus, ScheduleView},
        foundation::identity::AgentId,
        foundation::persistence::PersistenceFailure,
        identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailProjection},
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::runtime::RuntimeKind,
    };
    use tower::ServiceExt;

    use super::*;

    fn user(id: &str) -> silverhand_backend_core::foundation::identity::UserId {
        silverhand_backend_core::foundation::identity::UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_owned(),
            display_name: "Base Name".to_owned(),
            runtime: RuntimeKind::Codex,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            host_id: "h1".to_owned(),
            host_display_label: None,
            host_name: None,
            description: None,
            session_scope: "agent".to_owned(),
            avatar_storage_key: None,
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    /// Facts whose creator is `creator`, visible to every id in `viewers`.
    fn facts(creator: &str, viewers: &[&str]) -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user(creator),
            [],
            viewers.iter().map(|v| user(v)).collect::<Vec<_>>(),
            MaterializationStatus::Active,
            projection(),
        )
    }

    fn view(id: &str, status: ScheduleStatus) -> ScheduleView {
        ScheduleView {
            id: silverhand_backend_core::conversation_messaging::messaging::schedule::ScheduleId::parse(id)
                .expect("schedule id"),
            owner_agent_id: agent("a1"),
            target_conversation_id: "c1".to_owned(),
            target_label: "#ops".to_owned(),
            title: "digest".to_owned(),
            message: "hi".to_owned(),
            spec: ScheduleSpec::Once { fire_at: 100 },
            next_fire_at: 100,
            status,
            fired_count: 0,
            last_fired_at: None,
            max_occurrences: None,
            until_at: None,
            failure_reason: None,
            cancelled_at: None,
            last_failure_reason: None,
            last_failure_at: None,
            consecutive_failure_count: 0,
            created_at: 1,
            updated_at: 2,
        }
    }

    #[derive(Clone)]
    enum DetailOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
    }

    #[derive(Clone)]
    enum ListOutcome {
        Views(Vec<ScheduleView>),
        Fails,
    }

    #[derive(Clone)]
    struct FakeReads {
        detail: DetailOutcome,
        list: ListOutcome,
        detail_calls: Arc<AtomicUsize>,
        list_calls: Arc<AtomicUsize>,
        last_query: Arc<Mutex<Option<ScheduleListQuery>>>,
    }

    impl FakeReads {
        fn new(detail: DetailOutcome, list: ListOutcome) -> Self {
            Self {
                detail,
                list,
                detail_calls: Arc::new(AtomicUsize::new(0)),
                list_calls: Arc::new(AtomicUsize::new(0)),
                last_query: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            self.detail_calls.fetch_add(1, SeqCst);
            match &self.detail {
                DetailOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                DetailOutcome::Absent => Ok(None),
                DetailOutcome::Fails => {
                    Err(PersistenceFailure::from_source(io::Error::other("boom")))
                }
            }
        }
    }

    impl ScheduleListReadModel for FakeReads {
        async fn list_owned_schedules(
            &self,
            query: &ScheduleListQuery,
        ) -> Result<Vec<ScheduleView>, PersistenceFailure> {
            self.list_calls.fetch_add(1, SeqCst);
            *self.last_query.lock().unwrap() = Some(query.clone());
            match &self.list {
                ListOutcome::Views(views) => Ok(views.clone()),
                ListOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                    "read boom",
                ))),
            }
        }
    }

    struct Reply {
        status: StatusCode,
        body: String,
        list_reads: usize,
    }

    async fn list_with(reads: FakeReads, query: &str, with_principal: bool) -> Reply {
        let list_calls = reads.list_calls.clone();
        let uri = if query.is_empty() {
            "/api/agents/a1/schedules".to_owned()
        } else {
            format!("/api/agents/a1/schedules?{query}")
        };
        let mut request = Request::get(&uri);
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        let response = router(reads.clone(), reads)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            list_reads: list_calls.load(SeqCst),
        }
    }

    fn parsed(body: &str) -> Value {
        serde_json::from_str::<Value>(body).unwrap()
    }

    fn creator_reads(list: ListOutcome) -> FakeReads {
        FakeReads::new(
            DetailOutcome::Present(Box::new(facts("viewer-1", &[]))),
            list,
        )
    }

    // ---- the floor runs before the query parse (deferred parse) ----

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_reads() {
        let reads = creator_reads(ListOutcome::Views(vec![]));
        let detail_calls = reads.detail_calls.clone();
        let reply = list_with(reads, "status=bogus", false).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(detail_calls.load(SeqCst), 0);
        assert_eq!(reply.list_reads, 0);
    }

    #[tokio::test]
    async fn an_invalid_query_on_an_absent_agent_is_the_404_not_a_400() {
        // The DEFERRED parse guarantee: the visibility floor decides first, so a
        // bad query never turns the surface into an existence oracle.
        let reads = FakeReads::new(DetailOutcome::Absent, ListOutcome::Views(vec![]));
        let reply = list_with(reads, "status=bogus&limit=99999", true).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(parsed(&reply.body), json!({ "error": "agent not found" }));
        assert_eq!(reply.list_reads, 0);
    }

    #[tokio::test]
    async fn an_invalid_query_on_a_forbidden_agent_is_the_403_not_a_400() {
        // viewer-1 can SEE it (a member) but did not create it.
        let reads = FakeReads::new(
            DetailOutcome::Present(Box::new(facts("creator-user", &["viewer-1"]))),
            ListOutcome::Views(vec![]),
        );
        let reply = list_with(reads, "limit=abc", true).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(parsed(&reply.body), json!({ "error": "forbidden" }));
        assert_eq!(reply.list_reads, 0);
    }

    #[tokio::test]
    async fn a_gate_read_failure_is_a_safe_500() {
        let reads = FakeReads::new(DetailOutcome::Fails, ListOutcome::Views(vec![]));
        let reply = list_with(reads, "", true).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.list_reads, 0);
    }

    // ---- past the floor: the closed query grammar ----

    #[tokio::test]
    async fn an_omitted_query_lists_all_statuses_at_the_default_limit() {
        let reads = creator_reads(ListOutcome::Views(vec![view(
            "s1",
            ScheduleStatus::Pending,
        )]));
        let last_query = reads.last_query.clone();
        let reply = list_with(reads, "", true).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.list_reads, 1);
        let query = last_query.lock().unwrap().clone().unwrap();
        assert_eq!(query.statuses, None);
        assert_eq!(query.limit.get(), 100);
        assert_eq!(query.owner_agent_id, agent("a1"));
        let body = parsed(&reply.body);
        assert_eq!(body["schedules"].as_array().unwrap().len(), 1);
        assert_eq!(body["schedules"][0]["status"], "pending");
    }

    #[tokio::test]
    async fn status_active_expands_to_pending_and_firing() {
        let reads = creator_reads(ListOutcome::Views(vec![]));
        let last_query = reads.last_query.clone();
        let reply = list_with(reads, "status=active&limit=200", true).await;
        assert_eq!(reply.status, StatusCode::OK);
        let query = last_query.lock().unwrap().clone().unwrap();
        assert_eq!(
            query.statuses,
            Some(vec![ScheduleStatus::Pending, ScheduleStatus::Firing])
        );
        assert_eq!(query.limit.get(), 200);
        assert_eq!(parsed(&reply.body), json!({ "schedules": [] }));
    }

    #[tokio::test]
    async fn an_empty_status_or_limit_value_falls_back_to_the_default() {
        let reads = creator_reads(ListOutcome::Views(vec![]));
        let last_query = reads.last_query.clone();
        let reply = list_with(reads, "status=&limit=", true).await;
        assert_eq!(reply.status, StatusCode::OK);
        let query = last_query.lock().unwrap().clone().unwrap();
        assert_eq!(query.statuses, None);
        assert_eq!(query.limit.get(), 100);
    }

    #[tokio::test]
    async fn a_status_outside_the_family_is_the_invalid_query_400() {
        let reads = creator_reads(ListOutcome::Views(vec![]));
        let reply = list_with(reads, "status=bogus", true).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(parsed(&reply.body), json!({ "error": "invalid query" }));
        assert_eq!(reply.list_reads, 0);
    }

    #[tokio::test]
    async fn a_limit_outside_the_bounds_or_not_an_integer_is_the_invalid_query_400() {
        for bad in ["0", "201", "-5", "abc", "1.5"] {
            let reads = creator_reads(ListOutcome::Views(vec![]));
            let reply = list_with(reads, &format!("limit={bad}"), true).await;
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "limit={bad} must be rejected"
            );
            assert_eq!(parsed(&reply.body), json!({ "error": "invalid query" }));
            assert_eq!(reply.list_reads, 0);
        }
    }

    #[tokio::test]
    async fn a_read_failure_after_the_floor_is_a_safe_500() {
        let reads = creator_reads(ListOutcome::Fails);
        let reply = list_with(reads, "status=done", true).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.list_reads, 1);
    }

    // ---- the query grammar is form-urlencoded (percent-decode, `+`, first-key) ----

    #[tokio::test]
    async fn an_encoded_status_value_is_decoded_before_the_family_check() {
        // `%61ctive` decodes to `active`; a hand-split-without-decode parser would
        // reject it as outside the family.
        let reads = creator_reads(ListOutcome::Views(vec![]));
        let last_query = reads.last_query.clone();
        let reply = list_with(reads, "status=%61ctive", true).await;
        assert_eq!(reply.status, StatusCode::OK);
        let query = last_query.lock().unwrap().clone().unwrap();
        assert_eq!(
            query.statuses,
            Some(vec![ScheduleStatus::Pending, ScheduleStatus::Firing])
        );
    }

    #[tokio::test]
    async fn an_encoded_limit_value_is_decoded_before_the_integer_parse() {
        // `%32%35` decodes to `25`.
        let reads = creator_reads(ListOutcome::Views(vec![]));
        let last_query = reads.last_query.clone();
        let reply = list_with(reads, "limit=%32%35", true).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(last_query.lock().unwrap().clone().unwrap().limit.get(), 25);
    }

    #[tokio::test]
    async fn an_encoded_key_is_decoded_and_still_matches() {
        // `stat%75s` decodes to the `status` key.
        let reads = creator_reads(ListOutcome::Views(vec![]));
        let last_query = reads.last_query.clone();
        let reply = list_with(reads, "stat%75s=done", true).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            last_query.lock().unwrap().clone().unwrap().statuses,
            Some(vec![ScheduleStatus::Done])
        );
    }

    #[tokio::test]
    async fn a_plus_in_a_status_value_is_a_space_and_leaves_the_family() {
        // Form decoding turns `+` into a space; `ac tive` is not a family member.
        let reads = creator_reads(ListOutcome::Views(vec![]));
        let reply = list_with(reads, "status=ac+tive", true).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(parsed(&reply.body), json!({ "error": "invalid query" }));
        assert_eq!(reply.list_reads, 0);
    }

    #[tokio::test]
    async fn a_repeated_key_takes_its_first_occurrence() {
        // Hono's `c.req.query` returns the first value; a later `paused`/`999` is
        // ignored, so the first `active`/`50` decide.
        let reads = creator_reads(ListOutcome::Views(vec![]));
        let last_query = reads.last_query.clone();
        let reply = list_with(
            reads,
            "status=active&status=paused&limit=50&limit=999",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let query = last_query.lock().unwrap().clone().unwrap();
        assert_eq!(
            query.statuses,
            Some(vec![ScheduleStatus::Pending, ScheduleStatus::Firing])
        );
        assert_eq!(query.limit.get(), 50);
    }

    #[tokio::test]
    async fn a_javascript_numeric_literal_limit_is_the_narrower_rust_400() {
        // Accepted-set deviation: TS `Number('1e2'|'0x10'|'1.0')` coerces; the Rust
        // face's strict base-10 `i64` refinement rejects them all as invalid query.
        for js_number in ["1e2", "0x10", "1.0"] {
            let reads = creator_reads(ListOutcome::Views(vec![]));
            let reply = list_with(reads, &format!("limit={js_number}"), true).await;
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "limit={js_number} is the narrower Rust refinement's 400"
            );
            assert_eq!(reply.list_reads, 0);
        }
    }
}
