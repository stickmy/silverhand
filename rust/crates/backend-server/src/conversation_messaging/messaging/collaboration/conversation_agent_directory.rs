//! Authenticated HTTP adapter for the committed conversation-agent roster.

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
    conversation_messaging::messaging::collaboration::conversation_agent_directory::{
        BoundConversationAgentDirectory, ConversationAgentDirectoryReadPort,
        ConversationAgentDirectoryRefusal,
    },
    foundation::identity::{ConversationId, UserId},
};

pub const CONVERSATION_AGENT_DIRECTORY_PATH: &str = "/api/channels/{id}/agents";

struct DirectoryState<P>(Arc<P>);

impl<P> Clone for DirectoryState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: ConversationAgentDirectoryReadPort + Send + Sync + 'static,
{
    Router::new()
        .route(CONVERSATION_AGENT_DIRECTORY_PATH, get(read::<P>))
        .with_state(DirectoryState(Arc::new(port)))
}

async fn read<P: ConversationAgentDirectoryReadPort>(
    State(state): State<DirectoryState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw_id): Path<String>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("conversation-agent-directory route reached without human principal");
        return internal();
    };
    let conversation = match ConversationId::parse(raw_id) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "channel id required"),
    };
    match state.0.read(&viewer, &conversation).await {
        Ok(Ok(value)) => match value.bind(&conversation) {
            Ok(value) => success(&value),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "conversation-agent-directory identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(ConversationAgentDirectoryRefusal::NotFound)) => {
            error(StatusCode::NOT_FOUND, "channel not found")
        }
        Ok(Err(ConversationAgentDirectoryRefusal::Forbidden)) => {
            error(StatusCode::FORBIDDEN, "forbidden")
        }
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "conversation-agent-directory projection failed"
            );
            internal()
        }
    }
}

fn success(value: &BoundConversationAgentDirectory) -> Response {
    let agents: Vec<Value> = value
        .members()
        .iter()
        .map(|member| {
            json!({
                "conversation_id": value.conversation().as_str(),
                "agent_id": member.agent().as_str(),
                "joined_at": member.joined_at().get(),
            })
        })
        .collect();
    Json(json!({"agents": agents})).into_response()
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
    use serde_json::Value;
    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::conversation_agent_directory::{
            ConversationAgentDirectory, ConversationAgentDirectoryReadPort,
            ConversationAgentDirectoryRefusal, ConversationAgentMember, JoinedAt,
        },
        foundation::identity::{AgentId, ConversationId, UserId},
    };
    use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
    use sqlx::{
        Connection, Executor, SqliteConnection, SqlitePool,
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    };
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct NeverCalled(std::sync::Arc<AtomicUsize>);

    impl ConversationAgentDirectoryReadPort for NeverCalled {
        type Error = Infallible;

        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<
            Result<ConversationAgentDirectory, ConversationAgentDirectoryRefusal>,
            Self::Error,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Err(ConversationAgentDirectoryRefusal::NotFound))
        }
    }

    #[derive(Clone)]
    struct ProjectedIdentity {
        directory_conversation: &'static str,
        member_conversation: &'static str,
    }

    impl ConversationAgentDirectoryReadPort for ProjectedIdentity {
        type Error = Infallible;

        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<
            Result<ConversationAgentDirectory, ConversationAgentDirectoryRefusal>,
            Self::Error,
        > {
            Ok(Ok(ConversationAgentDirectory::projected(
                ConversationId::parse(self.directory_conversation).unwrap(),
                vec![ConversationAgentMember::projected(
                    ConversationId::parse(self.member_conversation).unwrap(),
                    AgentId::parse("agent-a").unwrap(),
                    JoinedAt::new(1).unwrap(),
                )],
            )
            .unwrap()))
        }
    }

    async fn call(port: ProjectedIdentity) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .uri("/api/channels/a/agents")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = router(port).oneshot(request).await.unwrap();
        let status = response.status();
        let body =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn auth_precedes_product_read_and_returned_identity_must_bind() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let response = router(NeverCalled(calls.clone()))
            .oneshot(
                Request::builder()
                    .uri("/api/channels/c/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let (status, body) = call(ProjectedIdentity {
            directory_conversation: "b",
            member_conversation: "b",
        })
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error":"internal error"}));
        let (status, body) = call(ProjectedIdentity {
            directory_conversation: "a",
            member_conversation: "b",
        })
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error":"internal error"}));
        let (status, body) = call(ProjectedIdentity {
            directory_conversation: "a",
            member_conversation: "a",
        })
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["agents"][0]["conversation_id"], "a");
    }

    async fn fixture_store(setup: &str) -> (tempfile::TempDir, MessagingStore, SqlitePool) {
        const SCHEMA: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation-agent-directory-wire.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
        sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('local','local-agent-directory','Local',1,1),('other','other-agent-directory','Other',1,1),('teammate','teammate-agent-directory','Teammate',1,1)")
            .execute(&mut db).await.unwrap();
        sqlx::query("INSERT INTO hosts(id,name,kind,created_at) VALUES('host','Host','local',1)")
            .execute(&mut db)
            .await
            .unwrap();
        if setup != "missing_conversation" {
            sqlx::query("INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','Workspace','conversation-agent-directory',1,1)")
                .execute(&mut db).await.unwrap();
            sqlx::query(
                "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','local',1)",
            )
            .execute(&mut db)
            .await
            .unwrap();
        }
        match setup {
            "direct_pair_endpoint" => {
                sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) VALUES('c',NULL,'direct_pair','fixed','human','local','human','other','',NULL,'idle','private',1,1)")
                    .execute(&mut db).await.unwrap();
            }
            "direct_pair_nonendpoint" => {
                sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) VALUES('c',NULL,'direct_pair','fixed','human','other','human','teammate','',NULL,'idle','private',1,1)")
                    .execute(&mut db).await.unwrap();
            }
            "missing_conversation" => {}
            other => {
                let visibility = if other == "public_workspace_nonmember" {
                    "public"
                } else {
                    "private"
                };
                let archived = if other == "archived_workspace_member" {
                    Some(9_i64)
                } else {
                    None
                };
                sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,archived_at,created_at,updated_at) VALUES('c','w','workspace','open','General','general-agent-directory','idle',?,?,1,1)")
                    .bind(visibility).bind(archived).execute(&mut db).await.unwrap();
                if matches!(other, "workspace_member" | "archived_workspace_member") {
                    sqlx::query("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1)")
                        .execute(&mut db).await.unwrap();
                    seed_agents(&mut db).await;
                }
            }
        }
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        let test_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        (dir, store, test_pool)
    }

    async fn seed_agents(db: &mut SqliteConnection) {
        for (id, name, created) in [
            ("agent-a", "agent-a-directory", 1_i64),
            ("agent-b", "agent-b-directory", 2_i64),
        ] {
            sqlx::query("INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) VALUES(?,?,'claude',?,?,?,'local')")
                .bind(id).bind(name).bind("host").bind(created).bind(created)
                .execute(&mut *db).await.unwrap();
        }
        sqlx::query("INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('c','agent-b',2),('c','agent-a',1)")
            .execute(&mut *db).await.unwrap();
        sqlx::query("DELETE FROM agent_runtime_slots WHERE agent_id IN ('agent-a','agent-b')")
            .execute(&mut *db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn production_sqlite_and_router_consume_the_ts_signed_surface() {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/fixtures/conversation-agent-directory-wire.json"
        )))
        .unwrap();
        for case in fixture.as_array().unwrap() {
            let (_dir, store, _test_pool) = fixture_store(case["setup"].as_str().unwrap()).await;
            let mut request = Request::builder()
                .uri("/api/channels/c/agents")
                .body(Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(UserId::parse("local").unwrap());
            let response = router(store.conversation_agent_directory())
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(
                response.status().as_u16(),
                case["status"].as_u64().unwrap() as u16,
                "{}",
                case["name"]
            );
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(body, case["body"], "{}", case["name"]);
        }
        assert_eq!(fixture.as_array().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn sqlite_total_order_and_corruption_gates_are_observable_at_wire() {
        let (_dir, store, test_pool) = fixture_store("workspace_member").await;
        sqlx::query("UPDATE conversation_agents SET joined_at=7")
            .execute(&test_pool)
            .await
            .unwrap();
        let mut request = Request::builder()
            .uri("/api/channels/c/agents")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = router(store.conversation_agent_directory())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["agents"][0]["agent_id"], "agent-a");
        assert_eq!(body["agents"][1]["agent_id"], "agent-b");

        sqlx::query("UPDATE conversation_agents SET joined_at=-1 WHERE agent_id='agent-a'")
            .execute(&test_pool)
            .await
            .unwrap();
        let mut request = Request::builder()
            .uri("/api/channels/c/agents")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = router(store.conversation_agent_directory())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        for (bad_agent, expected_source) in [
            ("", "stored conversation-agent identity is malformed"),
            (
                "missing-agent",
                "stored conversation-agent row does not reference a live agent",
            ),
        ] {
            let (_dir, store, test_pool) = fixture_store("workspace_member").await;
            sqlx::query("PRAGMA foreign_keys=OFF")
                .execute(&test_pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM conversation_agents")
                .execute(&test_pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) \
                 VALUES('c',?,1)",
            )
            .bind(bad_agent)
            .execute(&test_pool)
            .await
            .unwrap();
            let error = store
                .conversation_agent_directory()
                .read(
                    &UserId::parse("local").unwrap(),
                    &ConversationId::parse("c").unwrap(),
                )
                .await
                .unwrap_err();
            assert!(
                crate::foundation::error::describe(&error).contains(expected_source),
                "{}",
                crate::foundation::error::describe(&error)
            );
        }
    }
}
