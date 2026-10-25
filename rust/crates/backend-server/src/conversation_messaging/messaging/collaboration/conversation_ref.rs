//! Authenticated HTTP adapter for the workspace-conversation reference face.

use std::sync::Arc;

use crate::foundation::http_error::json_error as error;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_ref::{
        BoundWorkspaceConversationRef, ConversationRefReadPort, ConversationRefRefusal,
    },
    foundation::identity::{ConversationId, UserId},
};

pub const CONVERSATION_REF_PATH: &str = "/api/conversations/{id}/ref";

struct RefState<P>(Arc<P>);
impl<P> Clone for RefState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: ConversationRefReadPort + Send + Sync + 'static,
{
    Router::new()
        .route(CONVERSATION_REF_PATH, get(resolve::<P>))
        .with_state(RefState(Arc::new(port)))
}

async fn resolve<P: ConversationRefReadPort>(
    State(state): State<RefState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw_id): Path<String>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("conversation-ref route reached without its human principal fact");
        return internal();
    };
    let conversation = match ConversationId::parse(raw_id) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "conversation id required"),
    };
    match state.0.resolve(&viewer, &conversation).await {
        Ok(Ok(value)) => match value.bind(&conversation) {
            Ok(value) => success(&value),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "conversation-ref identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(ConversationRefRefusal::NotFound)) => {
            error(StatusCode::NOT_FOUND, "conversation not found")
        }
        Ok(Err(ConversationRefRefusal::Forbidden)) => error(StatusCode::FORBIDDEN, "forbidden"),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "conversation-ref projection failed"
            );
            internal()
        }
    }
}

fn success(value: &BoundWorkspaceConversationRef) -> Response {
    Json(json!({"ref": {
        "id": value.conversation().as_str(),
        "placement_kind": "workspace",
        "roster_policy": value.roster().as_str(),
        "title": value.title(),
        "workspace_id": value.workspace().as_str(),
        "closed_at": value.closed_at(),
        "archived_at": value.archived_at(),
    }}))
    .into_response()
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
        conversation_messaging::messaging::collaboration::conversation_ref::{
            ConversationRefReadPort, ConversationRefRefusal, ConversationRosterPolicy,
            WorkspaceConversationRef,
        },
        foundation::identity::{ConversationId, UserId, WorkspaceId},
    };
    use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
    use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct NeverCalled(std::sync::Arc<AtomicUsize>);

    impl ConversationRefReadPort for NeverCalled {
        type Error = Infallible;

        async fn resolve(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<Result<WorkspaceConversationRef, ConversationRefRefusal>, Self::Error> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Err(ConversationRefRefusal::NotFound))
        }
    }

    #[derive(Clone)]
    struct ProjectedIdentity(&'static str);

    impl ConversationRefReadPort for ProjectedIdentity {
        type Error = Infallible;

        async fn resolve(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<Result<WorkspaceConversationRef, ConversationRefRefusal>, Self::Error> {
            Ok(Ok(WorkspaceConversationRef::projected(
                ConversationId::parse(self.0).unwrap(),
                ConversationRosterPolicy::Open,
                format!("Title for {}", self.0),
                WorkspaceId::parse("w").unwrap(),
                Some(3),
                Some(7),
            )))
        }
    }

    #[tokio::test]
    async fn missing_principal_fails_before_product_read() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let response = router(NeverCalled(calls.clone()))
            .oneshot(
                Request::builder()
                    .uri("/api/conversations/c/ref")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    async fn call_identity(port: ProjectedIdentity) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .uri("/api/conversations/a/ref")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("viewer").unwrap());
        let response = router(port).oneshot(request).await.unwrap();
        let status = response.status();
        let body =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn returned_projection_must_match_the_requested_conversation() {
        let (status, body) = call_identity(ProjectedIdentity("b")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error":"internal error"}));

        let (status, body) = call_identity(ProjectedIdentity("a")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"ref": {
                "id": "a", "placement_kind": "workspace", "roster_policy": "open",
                "title": "Title for a", "workspace_id": "w", "closed_at": 3,
                "archived_at": 7
            }})
        );
    }

    async fn fixture_store(setup: &str) -> (tempfile::TempDir, MessagingStore) {
        const SCHEMA: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation-ref-wire.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
        sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('local','local-ref-wire','Local',1,1),('other','other-ref-wire','Other',1,1),('teammate','teammate-ref-wire','Teammate',1,1)")
            .execute(&mut db).await.unwrap();
        if setup != "missing_conversation" {
            sqlx::query("INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','Workspace','conversation-ref-wire',1,1)")
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
                let roster = if other == "fixed_workspace_nonmember" {
                    "fixed"
                } else {
                    "open"
                };
                let visibility = if matches!(
                    other,
                    "public_workspace_nonmember" | "archived_public_nonmember"
                ) {
                    "public"
                } else {
                    "private"
                };
                let archived = if matches!(other, "archived_public_nonmember" | "archived_member") {
                    Some(9_i64)
                } else {
                    None
                };
                sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,archived_at,created_at,updated_at) VALUES('c','w','workspace',?,'General','general-ref-wire','idle',?,?,1,1)")
                    .bind(roster).bind(visibility).bind(archived).execute(&mut db).await.unwrap();
                if matches!(other, "member_active_open" | "archived_member") {
                    sqlx::query("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1)")
                        .execute(&mut db).await.unwrap();
                }
            }
        }
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn production_sqlite_and_router_consume_the_ts_signed_ref_surface() {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/fixtures/conversation-ref-wire.json"
        )))
        .unwrap();
        for case in fixture.as_array().unwrap() {
            let (_dir, store) = fixture_store(case["setup"].as_str().unwrap()).await;
            let mut request = Request::builder()
                .uri("/api/conversations/c/ref")
                .body(Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(UserId::parse("local").unwrap());
            let response = router(store.conversation_refs())
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
        assert_eq!(fixture.as_array().unwrap().len(), 9);
    }
}
