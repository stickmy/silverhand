//! Authenticated HTTP adapter for the viewer's Activity cold projection.

use axum::{
    Json, Router,
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::activity::{
        ActivityItem, ActivityList, ActivityLocation, ActivityReadModel, ActivitySalience,
        ActivitySource,
    },
    foundation::identity::{UserId, WorkspaceId},
};
use std::sync::Arc;

use crate::{
    conversation_messaging::direct_pairs::counterpart_dto_value,
    conversation_messaging::messaging::author_wire::author_dto,
};

pub mod observe;

pub const ACTIVITY_PATH: &str = "/api/activity";

struct ActivityState<R>(Arc<R>);
impl<R> Clone for ActivityState<R> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<R>(read_model: R) -> Router
where
    R: ActivityReadModel + Send + Sync + 'static,
{
    Router::new()
        .route(ACTIVITY_PATH, get(list::<R>))
        .with_state(ActivityState(Arc::new(read_model)))
}

async fn list<R: ActivityReadModel>(
    State(state): State<ActivityState<R>>,
    principal: Option<Extension<UserId>>,
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("Activity route reached without its human principal fact");
        return internal();
    };
    // Hono's query(name) returns the first occurrence; the empty string is the
    // same as omission. Non-empty opaque coordinates remain valid filters and
    // produce an empty list if no visible conversation names them.
    let workspace = query
        .into_iter()
        .find_map(|(key, value)| (key == "workspace_id").then_some(value))
        .filter(|value| !value.is_empty())
        .and_then(|value| WorkspaceId::parse(value).ok());
    match state.0.list_activity(&viewer, workspace.as_ref()).await {
        Ok(value) => activity_wire(&value),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "Activity projection failed"
            );
            internal()
        }
    }
}

fn activity_wire(value: &ActivityList) -> Response {
    Json(json!({
        "items": value.items().iter().map(item_wire).collect::<Vec<_>>()
    }))
    .into_response()
}

fn item_wire(item: &ActivityItem) -> Value {
    let (id, kind, message_id, notification_id, unread_count) = match item.source() {
        ActivitySource::Mention {
            notification,
            message,
        } => (
            format!("mention:{}", notification.as_str()),
            "mention",
            Value::from(message.as_str()),
            Value::from(notification.as_str()),
            Value::Null,
        ),
        ActivitySource::AgentFailed { notification } => (
            format!("agent_failed:{}", notification.as_str()),
            "agent_failed",
            Value::Null,
            Value::from(notification.as_str()),
            Value::Null,
        ),
        ActivitySource::Unread { message, count } => (
            format!(
                "unread:{}::{}",
                item.conversation().as_str(),
                item.thread_root().map_or("", |root| root.as_str())
            ),
            "unread",
            Value::from(message.as_str()),
            Value::Null,
            Value::from(count.get()),
        ),
    };
    let salience = match item.salience() {
        ActivitySalience::Directed => "directed",
        ActivitySalience::Ambient => "ambient",
    };
    let location = match item.location() {
        ActivityLocation::WorkspaceChannel { workspace, title } => json!({
            "kind": "workspace_channel",
            "workspace_id": workspace.as_str(),
            "title": title,
        }),
        ActivityLocation::DirectPair { counterpart } => json!({
            "kind": "direct_pair",
            "counterpart": counterpart_dto_value(counterpart),
        }),
    };
    json!({
        "id": id,
        "kind": kind,
        "salience": salience,
        "conversation_id": item.conversation().as_str(),
        "thread_root_id": item.thread_root().map(|root| root.as_str()),
        "message_id": message_id,
        "notification_id": notification_id,
        "location": location,
        "thread_root_preview": item.thread_root_preview().map(|preview| preview.as_str()),
        "preview": item.preview().map(|preview| preview.as_str()),
        "preview_sender": item.preview_sender().map(author_dto),
        "timestamp": item.timestamp_ms(),
        "unread_count": unread_count,
    })
}

fn internal() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "internal error"})),
    )
        .into_response()
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
        conversation_messaging::messaging::{
            NotificationId,
            collaboration::activity::{
                ActivityItem, ActivityList, ActivityLocation, ActivityReadModel,
            },
        },
        foundation::identity::{ConversationId, WorkspaceId},
    };
    use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
    use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct Read {
        calls: Arc<AtomicUsize>,
    }
    impl ActivityReadModel for Read {
        type Error = Infallible;
        async fn list_activity(
            &self,
            _: &UserId,
            workspace: Option<&WorkspaceId>,
        ) -> Result<ActivityList, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let location = ActivityLocation::workspace_channel(
                workspace
                    .cloned()
                    .unwrap_or_else(|| WorkspaceId::parse("w-1").unwrap()),
                "General".to_owned(),
            );
            Ok(ActivityList::projected(vec![ActivityItem::agent_failed(
                NotificationId::parse("n-1").unwrap(),
                ConversationId::parse("c-1").unwrap(),
                location,
                None,
                7,
            )]))
        }
    }

    #[tokio::test]
    async fn auth_precedes_read_and_omitted_or_empty_first_filter_is_global() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = router(Read {
            calls: Arc::clone(&calls),
        });
        let unauth = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(ACTIVITY_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        for uri in [
            "/api/activity",
            "/api/activity?workspace_id=&workspace_id=w-second",
        ] {
            let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
            request
                .extensions_mut()
                .insert(UserId::parse("viewer").unwrap());
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(body["items"][0]["location"]["workspace_id"], "w-1", "{uri}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    async fn fixture_store(setup: &str) -> (tempfile::TempDir, MessagingStore) {
        const SCHEMA: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity-wire.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
        sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('local','local-activity','Local',1,1)")
            .execute(&mut db).await.unwrap();
        if setup == "pair" {
            for sql in [
                "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('activity-other','other-activity','Other',1,2)",
                "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,created_at,updated_at,latest_message_seq,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id) VALUES('pair',NULL,'direct_pair','fixed','',NULL,2,2,1,'human','activity-other','human','local')",
                "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,author_id,author_name,author_display_name) VALUES('m-pair','pair','user','activity-other','pair unread',1,2,'activity-other','other-activity','Other')",
                "INSERT INTO conversation_stream_heads(conversation_id,thread_root_id,latest_seq) VALUES('pair',NULL,1)",
            ] {
                sqlx::query(sql).execute(&mut db).await.unwrap();
            }
        } else if setup == "workspace" {
            for sql in [
                "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('activity-other','other-activity','Other',1,2)",
                "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','Workspace','workspace-activity',1,1)",
                "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','local',1)",
                "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,created_at,updated_at,latest_message_seq) VALUES('c','w','workspace','open','General','general-activity',1,1,2)",
                "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1)",
                "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,author_id,author_name,author_display_name) VALUES('m-mention','c','user','activity-other','  directed   text  ',1,1,'activity-other','other-activity','Other'),('m-plain','c','user','activity-other','plain unread',2,2,'activity-other','other-activity','Other')",
                "INSERT INTO conversation_stream_heads(conversation_id,thread_root_id,latest_seq) VALUES('c',NULL,2)",
                "INSERT INTO notifications(id,kind,user_id,workspace_id,conversation_id,message_id,payload,created_at) VALUES('n-old','mention_self','local','w','c','m-mention','{}',2),('n-new','mention_self','local','w','c','m-mention','{}',3),('n-fail','agent_start_failed','local','w','c',NULL,'{\"agent_name\":\"Alice\",\"error\":\"runtime down\"}',4)",
            ] {
                sqlx::query(sql).execute(&mut db).await.unwrap();
            }
        }
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn production_sqlite_and_router_consume_the_ts_signed_activity_surface() {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/fixtures/activity-wire.json"
        )))
        .unwrap();
        for case in fixture.as_array().unwrap() {
            let setup = case["setup"].as_str().unwrap();
            let (_dir, store) = fixture_store(setup).await;
            let app = router(store.activity_read_model());
            let mut request = Request::builder()
                .uri(case["path"].as_str().unwrap())
                .body(Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(UserId::parse("local").unwrap());
            let response = app.oneshot(request).await.unwrap();
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
    }
}
