//! Owner-only management-plane routes for per-agent custom environment variables:
//! `GET /api/agents/{id}/env` and `PUT /api/agents/{id}/env`.
//!
//! Authority is strictly human-creator only ([`can_manage_agent_env`]), layered
//! behind the active-only visibility gate ([`can_view_agent`]) so callers who
//! cannot view the agent get `404 {"error":"agent not found"}`, co-workspace
//! teammates get `403 {"error":"forbidden"}`, and agent tokens never reach this
//! human-session perimeter. Saving does not broadcast `agent_changed` and does
//! not restart live runtimes; updated values take effect on the next start.

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    foundation::identity::{AgentId, Principal, UserId},
    identity_access::{
        agent_detail_read::{AgentDetailFacts, AgentDetailReadModel},
        agent_env::{AgentEnvStore, AgentEnvVar, AgentEnvWriteOutcome, validate_agent_env_vars},
        capability::{
            agent_authority::{AgentAuthority, can_manage_agent_env},
            agent_view::{VisibleStages, can_view_agent},
        },
    },
};

use crate::foundation::{error, wire_admission, wire_admission::ValidatedJson};

pub const AGENT_ENV_PATH: &str = "/api/agents/{id}/env";

const AGENT_NOT_FOUND: &str = "agent not found";
const FORBIDDEN: &str = "forbidden";
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

#[derive(Clone)]
struct EnvState<R, S> {
    reads: R,
    store: S,
}

pub fn router<R, S>(reads: R, store: S) -> Router
where
    R: AgentDetailReadModel + Clone + Send + Sync + 'static,
    S: AgentEnvStore + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            AGENT_ENV_PATH,
            get(read_env::<R, S>).put(replace_env::<R, S>),
        )
        .with_state(EnvState { reads, store })
}

#[derive(Debug, Deserialize, Serialize)]
struct EnvVarWire {
    name: String,
    value: String,
}

#[derive(Debug, Serialize)]
struct EnvResponseWire {
    env_vars: Vec<EnvVarWire>,
}

#[derive(Debug, Deserialize, Validate)]
struct ReplaceEnvRequest {
    #[garde(skip)]
    env_vars: Vec<EnvVarWire>,
}

impl From<AgentEnvVar> for EnvVarWire {
    fn from(var: AgentEnvVar) -> Self {
        Self {
            name: var.name,
            value: var.value,
        }
    }
}

async fn authorize_owner<R>(
    reads: &R,
    viewer: UserId,
    agent_id: String,
) -> Result<AgentId, Box<Response>>
where
    R: AgentDetailReadModel + Send + Sync,
{
    let principal = Principal::human(viewer);

    let Ok(agent) = AgentId::parse(agent_id) else {
        return Err(Box::new(not_found()));
    };

    let detail = match reads.read_agent_detail(&agent).await {
        Ok(detail) => detail,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-env pre-dispatch read failed"
            );
            return Err(Box::new(internal_error()));
        }
    };

    if let Some(detail) = detail.as_ref()
        && detail.source() != &agent
    {
        tracing::error!(
            requested = agent.as_str(),
            resolved = detail.source().as_str(),
            "agent-env resolved for the wrong agent"
        );
        return Err(Box::new(internal_error()));
    }

    let decision = can_view_agent(
        &principal,
        &agent,
        detail.as_ref().map(AgentDetailFacts::visibility),
        &VisibleStages::active_only(),
    );
    let Some(detail) = detail.filter(|_| decision.is_allowed()) else {
        return Err(Box::new(not_found()));
    };

    let authority = AgentAuthority {
        created_by_user_id: Some(detail.created_by_user_id().clone()),
    };
    if !can_manage_agent_env(&principal, Some(&authority)).is_allowed() {
        return Err(Box::new(forbidden()));
    }

    Ok(agent)
}

async fn read_env<R, S>(
    State(state): State<EnvState<R, S>>,
    Extension(viewer): Extension<UserId>,
    Path(agent_id): Path<String>,
) -> Response
where
    R: AgentDetailReadModel + Send + Sync,
    S: AgentEnvStore + Send + Sync,
{
    let agent = match authorize_owner(&state.reads, viewer, agent_id).await {
        Ok(agent) => agent,
        Err(response) => return *response,
    };
    match state.store.read_agent_env(&agent).await {
        Ok(vars) => Json(EnvResponseWire {
            env_vars: vars.into_iter().map(EnvVarWire::from).collect(),
        })
        .into_response(),
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-env read failed"
            );
            internal_error()
        }
    }
}

async fn replace_env<R, S>(
    State(state): State<EnvState<R, S>>,
    Extension(viewer): Extension<UserId>,
    Path(agent_id): Path<String>,
    ValidatedJson(raw): ValidatedJson<ReplaceEnvRequest>,
) -> Response
where
    R: AgentDetailReadModel + Send + Sync,
    S: AgentEnvStore + Send + Sync,
{
    let vars: Vec<AgentEnvVar> = raw
        .env_vars
        .into_iter()
        .map(|entry| AgentEnvVar {
            name: entry.name,
            value: entry.value,
        })
        .collect();
    if validate_agent_env_vars(&vars).is_err() {
        return wire_admission::invalid_input();
    }

    let agent = match authorize_owner(&state.reads, viewer, agent_id).await {
        Ok(agent) => agent,
        Err(response) => return *response,
    };

    match state.store.replace_agent_env(&agent, &vars).await {
        Ok(AgentEnvWriteOutcome::NoSuchAgent) => not_found(),
        Ok(AgentEnvWriteOutcome::Replaced(saved)) => Json(EnvResponseWire {
            env_vars: saved.into_iter().map(EnvVarWire::from).collect(),
        })
        .into_response(),
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-env write failed"
            );
            internal_error()
        }
    }
}

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

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Json(ErrorBody { error: FORBIDDEN })).into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header::CONTENT_TYPE},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        foundation::persistence::PersistenceFailure,
        identity_access::{
            agent_detail_read::AgentDetailProjection, agent_lifecycle::MaterializationStatus,
            agents::runtime::RuntimeKind,
        },
    };
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_owned(),
            display_name: "Agent One".to_owned(),
            runtime: RuntimeKind::Antigravity,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            host_id: "h1".to_owned(),
            host_display_label: Some("Host One".to_owned()),
            host_name: Some("host-one".to_owned()),
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

    #[derive(Clone)]
    struct FakeReads(Option<AgentDetailFacts>);

    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            Ok(self.0.clone())
        }
    }

    #[derive(Clone, Default)]
    struct FakeStore(Arc<Mutex<Vec<AgentEnvVar>>>);

    impl AgentEnvStore for FakeStore {
        async fn read_agent_env(
            &self,
            _agent: &AgentId,
        ) -> Result<Vec<AgentEnvVar>, PersistenceFailure> {
            Ok(self.0.lock().expect("lock").clone())
        }

        async fn replace_agent_env(
            &self,
            _agent: &AgentId,
            vars: &[AgentEnvVar],
        ) -> Result<AgentEnvWriteOutcome, PersistenceFailure> {
            let mut guard = self.0.lock().expect("lock");
            *guard = vars.to_vec();
            Ok(AgentEnvWriteOutcome::Replaced(vars.to_vec()))
        }
    }

    async fn call(
        app: Router,
        method: &str,
        viewer: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri("/api/agents/a1/env");
        let body = match body {
            Some(v) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let mut req = builder.body(body).expect("request");
        req.extensions_mut().insert(user(viewer));
        let res = app.oneshot(req).await.expect("response");
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.expect("bytes");
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn owner_can_write_and_read_ordered_env_vars() {
        let facts = AgentDetailFacts::from_source(
            agent("a1"),
            user("owner-1"),
            [],
            [],
            MaterializationStatus::Active,
            projection(),
        );
        let store = FakeStore::default();
        let app = router(FakeReads(Some(facts)), store);

        let (status, body) = call(
            app.clone(),
            "PUT",
            "owner-1",
            Some(json!({
                "env_vars": [
                    { "name": "SILVERHAND_ACP_AUTO_COMPACT", "value": "200000" },
                    { "name": "CUSTOM_MARKER", "value": "marker-value-123" }
                ]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "env_vars": [
                    { "name": "SILVERHAND_ACP_AUTO_COMPACT", "value": "200000" },
                    { "name": "CUSTOM_MARKER", "value": "marker-value-123" }
                ]
            })
        );

        let (status, body) = call(app, "GET", "owner-1", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["env_vars"][1]["value"], "marker-value-123");
    }

    #[tokio::test]
    async fn co_workspace_member_gets_403_and_stranger_gets_404_on_get_and_put() {
        let facts = AgentDetailFacts::from_source(
            agent("a1"),
            user("owner-1"),
            [],
            [user("teammate-1")],
            MaterializationStatus::Active,
            projection(),
        );
        let store = FakeStore(Arc::new(Mutex::new(vec![AgentEnvVar {
            name: "MARKER".to_owned(),
            value: "never-leak".to_owned(),
        }])));
        let app = router(FakeReads(Some(facts)), store);

        let (st_get_team, body_get_team) = call(app.clone(), "GET", "teammate-1", None).await;
        assert_eq!(st_get_team, StatusCode::FORBIDDEN);
        assert_eq!(body_get_team, json!({ "error": "forbidden" }));

        let (st_put_team, _) = call(
            app.clone(),
            "PUT",
            "teammate-1",
            Some(json!({ "env_vars": [] })),
        )
        .await;
        assert_eq!(st_put_team, StatusCode::FORBIDDEN);

        let (st_get_stranger, body_get_stranger) = call(app.clone(), "GET", "stranger", None).await;
        assert_eq!(st_get_stranger, StatusCode::NOT_FOUND);
        assert_eq!(body_get_stranger, json!({ "error": "agent not found" }));

        let (st_put_stranger, _) =
            call(app, "PUT", "stranger", Some(json!({ "env_vars": [] }))).await;
        assert_eq!(st_put_stranger, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rejects_invalid_or_duplicate_env_names_with_422() {
        let facts = AgentDetailFacts::from_source(
            agent("a1"),
            user("owner-1"),
            [],
            [],
            MaterializationStatus::Active,
            projection(),
        );
        let app = router(FakeReads(Some(facts)), FakeStore::default());

        let (st, _) = call(
            app.clone(),
            "PUT",
            "owner-1",
            Some(json!({ "env_vars": [{ "name": "BAD-NAME", "value": "1" }] })),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

        let (st, _) = call(
            app,
            "PUT",
            "owner-1",
            Some(json!({
                "env_vars": [
                    { "name": "DUP", "value": "1" },
                    { "name": "DUP", "value": "2" }
                ]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    }
}
