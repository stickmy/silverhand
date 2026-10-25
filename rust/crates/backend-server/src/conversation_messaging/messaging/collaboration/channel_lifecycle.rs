//! Authenticated HTTP adapter for channel detail and lifecycle operations.
use crate::foundation::http_error::json_error as error;

mod mutation;
pub use mutation::mutation_router;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::channel_lifecycle::{
        BoundChannelDetail, ChannelDetailReadPort, ChannelDetailRefusal, ChannelLifecyclePlacement,
        ChannelLifecycleRow,
    },
    foundation::identity::{ConversationId, UserId},
};

pub const CHANNEL_DETAIL_PATH: &str = "/api/channels/{id}";

struct DetailState<P>(Arc<P>);
impl<P> Clone for DetailState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: ChannelDetailReadPort + Send + Sync + 'static,
{
    Router::new()
        .route(CHANNEL_DETAIL_PATH, get(detail::<P>))
        .with_state(DetailState(Arc::new(port)))
}

async fn detail<P: ChannelDetailReadPort>(
    State(state): State<DetailState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw_id): Path<String>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("channel-detail route reached without its human principal fact");
        return internal();
    };
    let conversation = match ConversationId::parse(raw_id) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "channel id required"),
    };
    match state.0.detail(&viewer, &conversation).await {
        Ok(Ok(value)) => match value.bind(&viewer, &conversation) {
            Ok(value) => success(&value),
            Err(failure) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&failure))
                    ),
                    "channel-detail identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(ChannelDetailRefusal::NotFound)) => {
            error(StatusCode::NOT_FOUND, "channel not found")
        }
        Ok(Err(ChannelDetailRefusal::Forbidden)) => error(StatusCode::FORBIDDEN, "forbidden"),
        Err(failure) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&failure))
                ),
                "channel-detail projection failed"
            );
            internal()
        }
    }
}

pub(super) fn success(value: &BoundChannelDetail) -> Response {
    let row = value.row();
    Json(json!({
        "channel": channel(row),
        "is_member": value.is_member(),
        "can_leave": value.leave().can_leave(),
        "leave_blocked_reason": value.leave().blocked_reason().map(|reason| reason.as_str()),
        "can_update_visibility": value.can_update_visibility(),
    }))
    .into_response()
}

fn channel(row: &ChannelLifecycleRow) -> serde_json::Value {
    let placement_kind = match row.placement() {
        ChannelLifecyclePlacement::Workspace(_) => "workspace",
        ChannelLifecyclePlacement::DirectPair(_) => "direct_pair",
    };
    json!({
        "id": row.conversation().as_str(),
        "workspace_id": row.workspace().map(|value| value.as_str()),
        "placement_kind": placement_kind,
        "roster_policy": row.roster().as_str(),
        "title": row.title(),
        "status": row.status().as_str(),
        "spawned_by_agent_id": row.spawned_by_agent().map(|value| value.as_str()),
        "closed_at": row.closed_at(),
        "close_summary": row.close_summary(),
        "archived_at": row.archived_at(),
        // Current production mapper exposes this column even though the shared
        // ConversationDto schema has not yet caught up with the runtime wire.
        "latest_message_seq": row.latest_message_seq(),
        "created_at": row.created_at(),
        "updated_at": row.updated_at(),
        "visibility": row.visibility().as_str(),
    })
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
        conversation_messaging::messaging::{
            collaboration::{
                channel_directory::{ChannelStatus, ChannelVisibility, DirectoryRoster},
                channel_lifecycle::{
                    ChannelDetailFact, ChannelDetailReadPort, ChannelDetailRefusal,
                    ChannelLifecycleRow,
                },
            },
            conversation_title::CanonicalConversationTitle,
        },
        foundation::identity::{ConversationId, UserId, WorkspaceId},
        identity_access::access::WorkspaceRole,
    };
    use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
    use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct ProjectedIdentity {
        viewer: &'static str,
        conversation: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl ChannelDetailReadPort for ProjectedIdentity {
        type Error = Infallible;

        async fn detail(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<Result<ChannelDetailFact, ChannelDetailRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let row = ChannelLifecycleRow::workspace_placed(
                ConversationId::parse(self.conversation).unwrap(),
                WorkspaceId::parse("w").unwrap(),
                DirectoryRoster::Open,
                CanonicalConversationTitle::parse("General").unwrap(),
                ChannelStatus::Idle,
                None,
                None,
                None,
                None,
                0,
                1,
                2,
                ChannelVisibility::Public,
            )
            .unwrap();
            Ok(Ok(ChannelDetailFact::projected(
                UserId::parse(self.viewer).unwrap(),
                row,
                false,
                Some(WorkspaceRole::Member),
                0,
            )
            .unwrap()))
        }
    }

    async fn call(port: ProjectedIdentity, with_principal: bool) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .uri("/api/channels/a")
            .body(Body::empty())
            .unwrap();
        if with_principal {
            request
                .extensions_mut()
                .insert(UserId::parse("viewer-a").unwrap());
        }
        let response = router(port).oneshot(request).await.unwrap();
        let status = response.status();
        let body =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn missing_principal_fails_before_product_read() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (status, body) = call(
            ProjectedIdentity {
                viewer: "viewer-a",
                conversation: "a",
                calls: calls.clone(),
            },
            false,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error":"internal error"}));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn viewer_and_conversation_coordinates_bind_independently() {
        for (viewer, conversation, expected) in [
            ("viewer-b", "a", StatusCode::INTERNAL_SERVER_ERROR),
            ("viewer-a", "b", StatusCode::INTERNAL_SERVER_ERROR),
            ("viewer-a", "a", StatusCode::OK),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let (status, _) = call(
                ProjectedIdentity {
                    viewer,
                    conversation,
                    calls: calls.clone(),
                },
                true,
            )
            .await;
            assert_eq!(status, expected);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    async fn fixture_store(setup: &str) -> (tempfile::TempDir, MessagingStore) {
        const SCHEMA: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("channel-lifecycle-detail-wire.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
        sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('local','local-lifecycle-wire','Local',1,1),('other','other-lifecycle-wire','Other',1,1),('teammate','teammate-lifecycle-wire','Teammate',1,1)")
            .execute(&mut db).await.unwrap();
        if setup != "missing" {
            sqlx::query("INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','Workspace','lifecycle-wire',1,1)")
                .execute(&mut db).await.unwrap();
            let role = if matches!(setup, "public_nonmember_admin" | "archived_member") {
                "admin"
            } else {
                "member"
            };
            sqlx::query("INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('w','local',1,?)")
                .bind(role).execute(&mut db).await.unwrap();
        }
        match setup {
            "missing" => {}
            "direct_pair_endpoint" | "direct_pair_foreign" => {
                let (a, b) = if setup == "direct_pair_endpoint" {
                    ("local", "other")
                } else {
                    ("other", "teammate")
                };
                sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) VALUES('c',NULL,'direct_pair','fixed','human',?,'human',?,'',NULL,'idle','private',1,2)")
                    .bind(a).bind(b).execute(&mut db).await.unwrap();
            }
            other => {
                let roster = if other == "fixed_member" {
                    "fixed"
                } else {
                    "open"
                };
                let visibility = if matches!(
                    other,
                    "public_nonmember_admin" | "public_member_owner" | "archived_member"
                ) {
                    "public"
                } else {
                    "private"
                };
                let archived = if other == "archived_member" {
                    Some(9_i64)
                } else {
                    None
                };
                sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,archived_at,created_at,updated_at) VALUES('c','w','workspace',?,'General','general','idle',?,?,1,2)")
                    .bind(roster).bind(visibility).bind(archived).execute(&mut db).await.unwrap();
                if other != "public_nonmember_admin" && other != "private_nonmember" {
                    sqlx::query("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1)")
                        .execute(&mut db).await.unwrap();
                }
                if matches!(other, "private_owner" | "public_member_owner") {
                    sqlx::query("UPDATE workspace_users SET role='owner' WHERE workspace_id='w' AND user_id='local'")
                        .execute(&mut db).await.unwrap();
                }
                if other == "private_owner" {
                    sqlx::query("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','other',1)")
                        .execute(&mut db).await.unwrap();
                }
            }
        }
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn production_sqlite_and_router_consume_the_ts_signed_detail_surface() {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/fixtures/channel-lifecycle-detail-wire.json"
        )))
        .unwrap();
        for case in fixture.as_array().unwrap() {
            let (_dir, store) = fixture_store(case["setup"].as_str().unwrap()).await;
            let mut request = Request::builder()
                .uri("/api/channels/c")
                .body(Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(UserId::parse("local").unwrap());
            let response = router(store.channel_lifecycle())
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
        assert_eq!(fixture.as_array().unwrap().len(), 10);
    }
}
