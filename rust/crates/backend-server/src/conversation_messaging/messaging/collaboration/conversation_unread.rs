//! Authenticated HTTP adapter for per-conversation stream unread.

use std::sync::Arc;

use crate::foundation::http_error::json_error as error;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Map, Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::conversation_unread::{
        BoundConversationUnread, ConversationUnreadReadPort, ConversationUnreadRefusal,
        StreamUnread,
    },
    foundation::identity::{ConversationId, UserId},
};

pub const CONVERSATION_UNREAD_PATH: &str = "/api/conversations/{id}/unread";

struct UnreadState<P>(Arc<P>);
impl<P> Clone for UnreadState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: ConversationUnreadReadPort + Send + Sync + 'static,
{
    Router::new()
        .route(CONVERSATION_UNREAD_PATH, get(read::<P>))
        .with_state(UnreadState(Arc::new(port)))
}

async fn read<P: ConversationUnreadReadPort>(
    State(state): State<UnreadState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw_id): Path<String>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("conversation-unread route reached without its human principal fact");
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
                    "conversation-unread identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(ConversationUnreadRefusal::NotFound)) => {
            error(StatusCode::NOT_FOUND, "channel not found")
        }
        Ok(Err(ConversationUnreadRefusal::Forbidden)) => error(StatusCode::FORBIDDEN, "forbidden"),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "conversation-unread projection failed"
            );
            internal()
        }
    }
}

fn success(value: &BoundConversationUnread) -> Response {
    let threads = value
        .threads()
        .iter()
        .map(|entry| {
            (
                entry.root().as_str().to_owned(),
                stream_wire(entry.unread()),
            )
        })
        .collect::<Map<String, Value>>();
    Json(json!({
        "mainline": stream_wire(value.mainline()),
        "threads": threads,
        "followed_thread_unread": value.followed_thread_unread(),
    }))
    .into_response()
}

fn stream_wire(value: &StreamUnread) -> Value {
    json!({
        "unread_count": value.count(),
        "first_unread_message_id": value.first_message().map(|id| id.as_str()),
        "first_unread_seq": value.first_seq().map(|seq| seq.get()),
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
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_unread::{
        ConversationUnread, ThreadUnread, UnreadCount,
    };
    use silverhand_backend_core::conversation_messaging::messaging::{MessageId, MessageSeq, ThreadRootId};
    use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
    use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct Read {
        returned_id: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl ConversationUnreadReadPort for Read {
        type Error = Infallible;

        async fn read(
            &self,
            _: &UserId,
            _: &ConversationId,
        ) -> Result<Result<ConversationUnread, ConversationUnreadRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let unread = StreamUnread::unread(
                UnreadCount::new(2).unwrap(),
                MessageId::parse("m1").unwrap(),
                MessageSeq::try_from(4).unwrap(),
            )
            .unwrap();
            Ok(Ok(ConversationUnread::projected(
                ConversationId::parse(self.returned_id).unwrap(),
                StreamUnread::caught_up(),
                vec![
                    ThreadUnread::projected(ThreadRootId::parse("root").unwrap(), unread).unwrap(),
                ],
                true,
            )
            .unwrap()))
        }
    }

    async fn call(port: Read, authenticated: bool) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .uri("/api/conversations/a/unread")
            .body(Body::empty())
            .unwrap();
        if authenticated {
            request
                .extensions_mut()
                .insert(UserId::parse("viewer").unwrap());
        }
        let response = router(port).oneshot(request).await.unwrap();
        let status = response.status();
        let body =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn auth_precedes_product_work_and_identity_is_bound_before_wire() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (status, _) = call(
            Read {
                returned_id: "a",
                calls: calls.clone(),
            },
            false,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let (status, body) = call(
            Read {
                returned_id: "b",
                calls: calls.clone(),
            },
            true,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error":"internal error"}));

        let (status, body) = call(
            Read {
                returned_id: "a",
                calls,
            },
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "mainline":{"unread_count":0,"first_unread_message_id":null,"first_unread_seq":null},
                "threads":{"root":{"unread_count":2,"first_unread_message_id":"m1","first_unread_seq":4}},
                "followed_thread_unread":true,
            })
        );
    }

    async fn fixture_store(setup: &str) -> (tempfile::TempDir, MessagingStore) {
        const SCHEMA: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation-unread-wire.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
        sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('local','local-unread-wire','Local',1,1),('other','other-unread-wire','Other',1,1),('teammate','teammate-unread-wire','Teammate',1,1)")
            .execute(&mut db).await.unwrap();
        if setup != "missing_conversation" {
            if setup.starts_with("direct_pair") {
                let (a, b) = if setup == "direct_pair_endpoint" {
                    ("local", "other")
                } else {
                    ("other", "teammate")
                };
                sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) VALUES('c',NULL,'direct_pair','fixed','human',?,'human',?,'',NULL,'idle','private',1,1)")
                    .bind(a).bind(b).execute(&mut db).await.unwrap();
            } else {
                sqlx::query("INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','Workspace','conversation-unread-wire',1,1)")
                    .execute(&mut db).await.unwrap();
                sqlx::query("INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','local',1)")
                    .execute(&mut db).await.unwrap();
                let visibility = if setup == "public_workspace_nonmember" {
                    "public"
                } else {
                    "private"
                };
                let archived = (setup == "archived_workspace_member").then_some(9_i64);
                sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,archived_at,created_at,updated_at) VALUES('c','w','workspace','open','General','general-unread-wire','idle',?,?,1,1)")
                    .bind(visibility).bind(archived).execute(&mut db).await.unwrap();
                if matches!(setup, "workspace_member" | "archived_workspace_member") {
                    sqlx::query("INSERT OR IGNORE INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1)")
                        .execute(&mut db).await.unwrap();
                } else {
                    sqlx::query("DELETE FROM conversation_users WHERE conversation_id='c' AND user_id='local'")
                        .execute(&mut db).await.unwrap();
                }
            }
            if !matches!(
                setup,
                "public_workspace_nonmember"
                    | "private_workspace_nonmember"
                    | "direct_pair_nonendpoint"
            ) {
                db.execute(sqlx::raw_sql("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,thread_root_id) VALUES('main-1','c','user','other','main first',1,1,NULL),('own-main','c','user','local','own',2,2,NULL),('system-main','c','system',NULL,'system',3,3,NULL),('root','c','user','other','root',4,4,NULL),('reply-1','c','user','other','reply one',5,5,'root'),('own-reply','c','user','local','own reply',6,6,'root'),('reply-2','c','user','other','reply two',7,7,'root'); INSERT INTO conversation_stream_heads(conversation_id,thread_root_id,latest_seq) VALUES('c',NULL,4),('c','root',7); INSERT INTO conversation_reads(user_id,conversation_id,thread_root_id,last_read_seq,read_at) VALUES('local','c',NULL,1,8),('local','c','root',5,8); INSERT INTO thread_followers(thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source) VALUES('root','c','human','local',8,'manual');"))
                    .await.unwrap();
            }
        }
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn production_sqlite_and_router_consume_the_ts_signed_surface() {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/fixtures/conversation-unread-wire.json"
        )))
        .unwrap();
        for case in fixture.as_array().unwrap() {
            let (_dir, store) = fixture_store(case["setup"].as_str().unwrap()).await;
            let mut request = Request::builder()
                .uri("/api/conversations/c/unread")
                .body(Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(UserId::parse("local").unwrap());
            let response = router(store.conversation_unread())
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
        assert_eq!(fixture.as_array().unwrap().len(), 7);
    }

    async fn corrupt(path: &std::path::Path, sql: &'static str) {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(false);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute("PRAGMA ignore_check_constraints=ON")
            .await
            .unwrap();
        db.execute("DROP TRIGGER IF EXISTS conversations_placement_immutable")
            .await
            .unwrap();
        sqlx::query(sql).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
    }

    async fn call_store(store: MessagingStore) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .uri("/api/conversations/c/unread")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = router(store.conversation_unread())
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let body =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn malformed_placement_or_selected_identity_fails_before_any_dto() {
        for mutation in [
            "UPDATE conversations SET workspace_id=NULL WHERE id='c'",
            "UPDATE messages SET id='' WHERE id='reply-2'",
            "UPDATE messages SET from_kind='bogus' WHERE id='reply-2'",
        ] {
            let (dir, store) = fixture_store("workspace_member").await;
            corrupt(
                &dir.path().join("conversation-unread-wire.sqlite"),
                mutation,
            )
            .await;
            let (status, body) = call_store(store).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{mutation}");
            assert_eq!(body, json!({"error":"internal error"}), "{mutation}");
        }

        let (dir, store) = fixture_store("direct_pair_endpoint").await;
        corrupt(
            &dir.path().join("conversation-unread-wire.sqlite"),
            "UPDATE conversations SET roster_policy='open' WHERE id='c'",
        )
        .await;
        let (status, body) = call_store(store).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error":"internal error"}));
    }

    #[tokio::test]
    async fn an_orphan_workspace_is_forbidden_even_when_both_membership_rows_survive() {
        let (dir, store) = fixture_store("workspace_member").await;
        // Foreign keys are deliberately disabled only for this falsifier: it
        // preserves both membership rows while removing the owning workspace,
        // the exact state that distinguishes the existence leg from the two
        // seat legs.
        corrupt(
            &dir.path().join("conversation-unread-wire.sqlite"),
            "DELETE FROM workspaces WHERE id='w'",
        )
        .await;
        let (status, body) = call_store(store).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"error":"forbidden"}));
    }
}
