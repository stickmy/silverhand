//! Authenticated HTTP adapter for committed-roster runtime projection.

use std::sync::Arc;

use crate::foundation::http_error::json_error as error;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_agent_runtime::{
        BoundConversationAgentRuntimeProjection, ConversationAgentRuntimeReadPort,
        ConversationAgentRuntimeRefusal,
    },
    foundation::identity::{ConversationId, UserId},
};

pub const CONVERSATION_AGENT_RUNTIME_PATH: &str = "/api/channels/{id}/agent-runtime";

struct RuntimeState<P>(Arc<P>);

impl<P> Clone for RuntimeState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: ConversationAgentRuntimeReadPort + Send + Sync + 'static,
{
    Router::new()
        .route(CONVERSATION_AGENT_RUNTIME_PATH, get(read::<P>))
        .with_state(RuntimeState(Arc::new(port)))
}

async fn read<P: ConversationAgentRuntimeReadPort>(
    State(state): State<RuntimeState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw_id): Path<String>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("conversation-agent-runtime route reached without human principal");
        return internal();
    };
    let conversation = match ConversationId::parse(raw_id) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "channel id required"),
    };
    match state.0.read(&viewer, &conversation).await {
        Ok(Ok(value)) => match value.bind(&viewer, &conversation) {
            Ok(value) => success(&value),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "conversation-agent-runtime identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(ConversationAgentRuntimeRefusal::NotFound)) => {
            error(StatusCode::NOT_FOUND, "channel not found")
        }
        Ok(Err(ConversationAgentRuntimeRefusal::Forbidden)) => {
            error(StatusCode::FORBIDDEN, "forbidden")
        }
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "conversation-agent-runtime projection failed"
            );
            internal()
        }
    }
}

fn success(value: &BoundConversationAgentRuntimeProjection) -> Response {
    let runtime: Vec<Value> = value
        .rows()
        .iter()
        .map(|row| {
            json!({
                "agent_id": row.agent().as_str(),
                "scope_kind": row.scope_kind(),
                "generation": row.generation().as_i64(),
                "cli_session_id": row.cli_session().map(|value| value.as_str()),
                "pid": row.pid().map(|value| value.get()),
                "status": row.status().as_str(),
            })
        })
        .collect();
    Json(json!({"runtime": runtime})).into_response()
}

fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::{
            CliSessionId,
            conversation_agent_runtime::{
                ConversationAgentRuntimeProjection, ConversationAgentRuntimeReadPort,
                ConversationAgentRuntimeRefusal, ConversationAgentRuntimeRow, RuntimePid,
                RuntimeProjectionStatus,
            },
        },
        foundation::identity::{AgentId, ConversationId, UserId},
        host_runtime::runtime_credential::{RuntimeGeneration, RuntimeScope},
    };
    use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
    use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct NeverCalled(std::sync::Arc<AtomicUsize>);

    impl ConversationAgentRuntimeReadPort for NeverCalled {
        type Error = Infallible;

        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<
            Result<ConversationAgentRuntimeProjection, ConversationAgentRuntimeRefusal>,
            Self::Error,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Err(ConversationAgentRuntimeRefusal::NotFound))
        }
    }

    #[derive(Clone)]
    struct ProjectedIdentity {
        viewer: &'static str,
        conversation: &'static str,
    }

    impl ConversationAgentRuntimeReadPort for ProjectedIdentity {
        type Error = Infallible;

        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<
            Result<ConversationAgentRuntimeProjection, ConversationAgentRuntimeRefusal>,
            Self::Error,
        > {
            let agent = AgentId::parse("agent-a").unwrap();
            Ok(Ok(ConversationAgentRuntimeProjection::projected(
                UserId::parse(self.viewer).unwrap(),
                ConversationId::parse(self.conversation).unwrap(),
                vec![
                    ConversationAgentRuntimeRow::projected(
                        ConversationId::parse(self.conversation).unwrap(),
                        agent.clone(),
                        RuntimeScope::AgentSlot { agent },
                        RuntimeGeneration::new(2).unwrap(),
                        Some(CliSessionId::parse("cli").unwrap()),
                        Some(RuntimePid::new(7).unwrap()),
                        RuntimeProjectionStatus::Active,
                    )
                    .unwrap(),
                ],
            )
            .unwrap()))
        }
    }

    async fn call(port: ProjectedIdentity) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .uri("/api/channels/c/agent-runtime")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("viewer-a").unwrap());
        let response = router(port).oneshot(request).await.unwrap();
        let status = response.status();
        let body =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn auth_and_returned_viewer_conversation_bind_before_wire() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let response = router(NeverCalled(calls.clone()))
            .oneshot(
                Request::builder()
                    .uri("/api/channels/c/agent-runtime")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        for (viewer, conversation) in [("viewer-b", "c"), ("viewer-a", "other")] {
            let (status, body) = call(ProjectedIdentity {
                viewer,
                conversation,
            })
            .await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(body, json!({"error":"internal error"}));
        }
        let (status, body) = call(ProjectedIdentity {
            viewer: "viewer-a",
            conversation: "c",
        })
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"runtime":[{
                "agent_id":"agent-a",
                "scope_kind":"agent",
                "generation":2,
                "cli_session_id":"cli",
                "pid":7,
                "status":"active"
            }]})
        );
    }

    async fn production_store() -> (tempfile::TempDir, MessagingStore) {
        const SCHEMA: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation-agent-runtime-wire.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
        for statement in [
            "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('viewer','runtime-wire-viewer','Viewer',1,1)",
            "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','Host','local',1)",
            "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,display_name,session_scope) VALUES \
             ('a-conversation','runtime-wire-conversation','codex','h','viewer',1,1,'Conversation','conversation'),\
             ('z-global','runtime-wire-global','codex','h','viewer',1,1,'Global','agent')",
            "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','Workspace','runtime-wire-workspace',1,1)",
            "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','viewer',1)",
            "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES('c','w','workspace','open','Runtime','runtime','idle','private',1,1)",
            "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','viewer',1)",
            "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('c','z-global',1),('c','a-conversation',1)",
            "UPDATE agent_runtime_slots SET observed_status='active',observed_pid=12,cli_session_id='conversation-cli' WHERE agent_id='a-conversation' AND conversation_id='c'",
            "UPDATE agent_runtime_slots SET observed_status='starting',observed_pid=NULL,cli_session_id=NULL WHERE agent_id='z-global' AND conversation_id IS NULL",
        ] {
            db.execute(statement).await.unwrap();
        }
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn production_sqlite_and_router_emit_the_existing_wire_shape() {
        let (_dir, store) = production_store().await;
        let mut request = Request::builder()
            .uri("/api/channels/c/agent-runtime")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("viewer").unwrap());
        let response = router(store.conversation_agent_runtime())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            body,
            json!({"runtime":[
                {"agent_id":"a-conversation","scope_kind":"conversation","generation":0,"cli_session_id":"conversation-cli","pid":12,"status":"active"},
                {"agent_id":"z-global","scope_kind":"agent","generation":0,"cli_session_id":null,"pid":null,"status":"starting"}
            ]})
        );
    }
}
