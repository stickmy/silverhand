//! Authenticated human thread-follow routes.

use std::sync::Arc;

use crate::foundation::http_error::json_error as error;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        ThreadRootId,
        collaboration::thread_follow::{
            BoundHumanFollow, BoundHumanUnfollow, BoundThreadFollowerState, HumanFollowRefusal,
            HumanUnfollowRefusal, ThreadFollowAftermath, ThreadFollowPort,
            ThreadFollowerStateRefusal, follow_human, unfollow_human,
        },
    },
    foundation::identity::{ConversationId, UserId},
};

pub const FOLLOWERS_PATH: &str = "/api/channels/{id}/threads/{root_message_id}/followers";
pub const FOLLOW_PATH: &str = "/api/channels/{id}/threads/{root_message_id}/follow";

struct Inner<P, A> {
    port: P,
    aftermath: A,
}

struct ThreadFollowState<P, A>(Arc<Inner<P, A>>);

impl<P, A> Clone for ThreadFollowState<P, A> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P, A>(port: P, aftermath: A) -> Router
where
    P: ThreadFollowPort + Send + Sync + 'static,
    A: ThreadFollowAftermath + Send + Sync + 'static,
{
    Router::new()
        .route(FOLLOWERS_PATH, get(read::<P, A>))
        .route(
            FOLLOW_PATH,
            axum::routing::post(follow::<P, A>).delete(unfollow::<P, A>),
        )
        .with_state(ThreadFollowState(Arc::new(Inner { port, aftermath })))
}

#[derive(Deserialize)]
struct ThreadPath {
    id: String,
    root_message_id: String,
}

fn coordinates(path: ThreadPath) -> Result<(ConversationId, ThreadRootId), ()> {
    let conversation = ConversationId::parse(path.id).map_err(|_| ())?;
    let root = ThreadRootId::parse(path.root_message_id).map_err(|_| ())?;
    Ok((conversation, root))
}

async fn read<P: ThreadFollowPort, A: ThreadFollowAftermath>(
    State(state): State<ThreadFollowState<P, A>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<ThreadPath>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("thread-follower state route reached without human principal");
        return internal();
    };
    let (conversation, message) = match coordinates(path) {
        Ok(value) => value,
        Err(()) => return missing_coordinates(),
    };
    match state.0.port.state(&viewer, &conversation, &message).await {
        Ok(Ok(value)) => match value.bind(&viewer, &conversation, &message) {
            Ok(value) => state_success(&value),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "thread-follower state identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(refusal)) => state_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "thread-follower state failed"
            );
            internal()
        }
    }
}

async fn follow<P: ThreadFollowPort, A: ThreadFollowAftermath>(
    State(state): State<ThreadFollowState<P, A>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<ThreadPath>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("thread-follow route reached without human principal");
        return internal();
    };
    let (conversation, message) = match coordinates(path) {
        Ok(value) => value,
        Err(()) => return missing_coordinates(),
    };
    match follow_human(
        &state.0.port,
        &state.0.aftermath,
        viewer,
        conversation,
        message,
    )
    .await
    {
        Ok(Ok(execution)) => {
            if let Some(failure) = execution.publication_failure() {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(failure))
                    ),
                    "committed thread-follow unread publication failed"
                );
            }
            follow_success(execution.committed())
        }
        Ok(Err(refusal)) => follow_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "thread-follow write failed"
            );
            internal()
        }
    }
}

async fn unfollow<P: ThreadFollowPort, A: ThreadFollowAftermath>(
    State(state): State<ThreadFollowState<P, A>>,
    principal: Option<Extension<UserId>>,
    Path(path): Path<ThreadPath>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("thread-unfollow route reached without human principal");
        return internal();
    };
    let (conversation, message) = match coordinates(path) {
        Ok(value) => value,
        Err(()) => return missing_coordinates(),
    };
    match unfollow_human(
        &state.0.port,
        &state.0.aftermath,
        viewer,
        conversation,
        message,
    )
    .await
    {
        Ok(Ok(execution)) => {
            if let Some(failure) = execution.publication_failure() {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(failure))
                    ),
                    "committed thread-unfollow unread publication failed"
                );
            }
            unfollow_success(execution.committed())
        }
        Ok(Err(refusal)) => unfollow_refusal(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "thread-unfollow write failed"
            );
            internal()
        }
    }
}

fn state_success(value: &BoundThreadFollowerState) -> Response {
    Json(json!({
        "thread_root_id": value.root().as_str(),
        "viewer_following": value.viewer_following(),
        "follower_agent_ids": value.agents().iter().map(|agent| agent.agent().as_str()).collect::<Vec<_>>(),
    }))
    .into_response()
}

fn follow_success(value: &BoundHumanFollow) -> Response {
    Json(json!({
        "thread_root_id": value.root().as_str(),
        "changed": value.changed(),
        "viewer_following": true,
    }))
    .into_response()
}

fn unfollow_success(value: &BoundHumanUnfollow) -> Response {
    Json(json!({
        "thread_root_id": value.root().as_str(),
        "changed": value.changed(),
        "viewer_following": false,
    }))
    .into_response()
}

fn state_refusal(refusal: ThreadFollowerStateRefusal) -> Response {
    match refusal {
        ThreadFollowerStateRefusal::ConversationNotFound => {
            error(StatusCode::NOT_FOUND, "channel not found")
        }
        ThreadFollowerStateRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        ThreadFollowerStateRefusal::RootNotFound => root_not_found(),
    }
}

fn follow_refusal(refusal: HumanFollowRefusal) -> Response {
    match refusal {
        HumanFollowRefusal::ConversationNotFound => {
            error(StatusCode::NOT_FOUND, "channel not found")
        }
        HumanFollowRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        HumanFollowRefusal::RootNotFound => root_not_found(),
    }
}

fn unfollow_refusal(refusal: HumanUnfollowRefusal) -> Response {
    match refusal {
        HumanUnfollowRefusal::ConversationNotFound => {
            error(StatusCode::NOT_FOUND, "channel not found")
        }
        HumanUnfollowRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        HumanUnfollowRefusal::RootNotFound => root_not_found(),
    }
}

fn root_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error":"thread root message not found in conversation",
            "code":"reply_target_not_found"
        })),
    )
        .into_response()
}

fn missing_coordinates() -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "channel id and root message id required",
    )
}

fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        io::{self, Write},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
    };
    use serde_json::Value;
    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::thread_follow::{
            CommittedHumanFollow, CommittedHumanUnfollow, FollowedAt, ThreadFollowChangedFact,
            ThreadFollowPublicationFailure, ThreadFollowerAgent, ThreadFollowerState,
        },
        foundation::identity::AgentId,
    };
    use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
    use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
    use tower::ServiceExt;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[derive(Clone, Default)]
    struct RecordingAftermath {
        calls: Arc<Mutex<Vec<(String, String)>>>,
        fail: bool,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("event hub unavailable")]
    struct HubDown {
        #[source]
        source: io::Error,
    }

    impl ThreadFollowAftermath for RecordingAftermath {
        async fn unread_changed(
            &self,
            fact: ThreadFollowChangedFact<'_>,
        ) -> Result<(), ThreadFollowPublicationFailure> {
            self.calls.lock().unwrap().push((
                fact.viewer().as_str().to_owned(),
                fact.conversation().as_str().to_owned(),
            ));
            if self.fail {
                Err(ThreadFollowPublicationFailure::from_source(HubDown {
                    source: io::Error::other("scope event socket closed"),
                }))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    struct LogWriter(LogBuffer);

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for LogBuffer {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            LogWriter(self.clone())
        }
    }

    impl LogBuffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[derive(Clone)]
    struct SplicedPort {
        state_conversation: &'static str,
        state_agent_conversation: &'static str,
        state_requested: &'static str,
        mutation_conversation: &'static str,
        mutation_requested: &'static str,
    }

    impl ThreadFollowPort for SplicedPort {
        type Error = Infallible;

        async fn state(
            &self,
            viewer: &UserId,
            _: &ConversationId,
            _: &ThreadRootId,
        ) -> Result<Result<ThreadFollowerState, ThreadFollowerStateRefusal>, Self::Error> {
            Ok(Ok(ThreadFollowerState::projected(
                viewer.clone(),
                ConversationId::parse(self.state_conversation).unwrap(),
                ThreadRootId::parse(self.state_requested).unwrap(),
                ConversationId::parse(self.state_agent_conversation).unwrap(),
                ThreadRootId::parse(self.mutation_requested).unwrap(),
                true,
                vec![ThreadFollowerAgent::projected(
                    ConversationId::parse(self.state_agent_conversation).unwrap(),
                    ThreadRootId::parse("root").unwrap(),
                    AgentId::parse("agent-a").unwrap(),
                    FollowedAt::new(1).unwrap(),
                )],
            )
            .unwrap()))
        }

        async fn follow(
            &self,
            viewer: UserId,
            _: ConversationId,
            _: ThreadRootId,
        ) -> Result<Result<CommittedHumanFollow, HumanFollowRefusal>, Self::Error> {
            Ok(Ok(CommittedHumanFollow::committed(
                viewer,
                ConversationId::parse(self.mutation_conversation).unwrap(),
                ThreadRootId::parse(self.mutation_requested).unwrap(),
                ConversationId::parse(self.state_agent_conversation).unwrap(),
                ThreadRootId::parse("root").unwrap(),
                true,
            )))
        }

        async fn unfollow(
            &self,
            viewer: UserId,
            _: ConversationId,
            _: ThreadRootId,
        ) -> Result<Result<CommittedHumanUnfollow, HumanUnfollowRefusal>, Self::Error> {
            Ok(Ok(CommittedHumanUnfollow::committed(
                viewer,
                ConversationId::parse(self.mutation_conversation).unwrap(),
                ThreadRootId::parse(self.mutation_requested).unwrap(),
                ConversationId::parse(self.state_agent_conversation).unwrap(),
                ThreadRootId::parse("root").unwrap(),
                true,
            )))
        }
    }

    async fn response(
        router: Router,
        method: Method,
        uri: &str,
        authenticated: bool,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        if authenticated {
            request
                .extensions_mut()
                .insert(UserId::parse("local").unwrap());
        }
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let body =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn auth_and_every_returned_coordinate_bind_before_wire_or_effect() {
        let aftermath = RecordingAftermath::default();
        let base = SplicedPort {
            state_conversation: "c",
            state_agent_conversation: "c",
            state_requested: "root",
            mutation_conversation: "c",
            mutation_requested: "root",
        };
        let (status, _) = response(
            router(base.clone(), aftermath.clone()),
            Method::GET,
            "/api/channels/c/threads/root/followers",
            false,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(aftermath.calls.lock().unwrap().is_empty());

        for port in [
            SplicedPort {
                state_conversation: "other",
                ..base.clone()
            },
            SplicedPort {
                state_agent_conversation: "other",
                ..base.clone()
            },
            SplicedPort {
                state_requested: "other",
                ..base.clone()
            },
        ] {
            let (status, body) = response(
                router(port, aftermath.clone()),
                Method::GET,
                "/api/channels/c/threads/root/followers",
                true,
            )
            .await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(body, json!({"error":"internal error"}));
        }
        let (status, _) = response(
            router(
                SplicedPort {
                    mutation_conversation: "other",
                    ..base.clone()
                },
                aftermath.clone(),
            ),
            Method::POST,
            "/api/channels/c/threads/root/follow",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(aftermath.calls.lock().unwrap().is_empty());
        let (status, _) = response(
            router(
                SplicedPort {
                    mutation_requested: "other",
                    ..base.clone()
                },
                aftermath.clone(),
            ),
            Method::DELETE,
            "/api/channels/c/threads/root/follow",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(aftermath.calls.lock().unwrap().is_empty());

        let (status, body) = response(
            router(base, aftermath.clone()),
            Method::GET,
            "/api/channels/c/threads/root/followers",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"thread_root_id":"root","viewer_following":true,"follower_agent_ids":["agent-a"]})
        );
    }

    async fn fixture_store(setup: &str) -> (tempfile::TempDir, MessagingStore) {
        const SCHEMA: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thread-follow.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
        if setup == "stale_direct_pair" {
            // The authenticated endpoint coordinate survives while its user row
            // does not.  Direct-pair participation alone must not turn this
            // broken post-validation identity into an idempotent success.
            db.execute(sqlx::raw_sql(
                "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES
                 ('local','local-thread-follow','Local',1,1),
                 ('other','other-thread-follow','Other',1,1);
                 INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,
                    endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,
                    title,title_key,status,visibility,created_at,updated_at)
                 VALUES('c',NULL,'direct_pair','fixed','human','local','human','other',
                    '',NULL,'idle','private',1,1);
                 INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,thread_root_id)
                 VALUES('root','c','user','other','root',1,1,NULL);",
            ))
            .await
            .unwrap();
            db.execute("PRAGMA foreign_keys=OFF").await.unwrap();
            sqlx::query("DELETE FROM users WHERE id='local'")
                .execute(&mut db)
                .await
                .unwrap();
        } else if setup != "missing_conversation" {
            db.execute(sqlx::raw_sql(
            "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES
             ('local','local-thread-follow','Local',1,1),('other','other-thread-follow','Other',1,1);
             INSERT INTO hosts(id,name,kind,created_at) VALUES('host','Host','local',1);
             INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) VALUES
             ('agent-a','agent-a-thread-follow','claude','host',1,1,'local'),
             ('agent-b','agent-b-thread-follow','claude','host',1,1,'local'),
             ('agent-x','agent-x-thread-follow','claude','host',1,1,'local');
             INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','Workspace','thread-follow',1,1);
             INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','local',1);
             INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at)
             VALUES('c','w','workspace','open','General','thread-follow','idle','private',1,1);"
            )).await.unwrap();
            if setup == "forbidden" {
                sqlx::query(
                    "DELETE FROM conversation_users WHERE conversation_id='c' AND user_id='local'",
                )
                .execute(&mut db)
                .await
                .unwrap();
            } else {
                db.execute(sqlx::raw_sql(
                    "INSERT OR IGNORE INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1);
                     INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('c','agent-a',1),('c','agent-b',1);
                     INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,thread_root_id) VALUES
                     ('root','c','user','local','root',1,1,NULL),('reply','c','user','other','reply',2,2,'root');"
                )).await.unwrap();
                if setup == "state_member" {
                    db.execute(sqlx::raw_sql(
                        "INSERT INTO thread_followers(thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source) VALUES
                         ('root','c','human','local',1,'manual'),('root','c','agent','agent-a',2,'mention'),
                         ('root','c','agent','agent-b',3,'reply'),('root','c','agent','agent-x',4,'manual');"
                    )).await.unwrap();
                } else if setup == "tie_order" {
                    db.execute(sqlx::raw_sql(
                        "INSERT INTO thread_followers(thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source) VALUES
                         ('root','c','agent','agent-b',7,'reply'),('root','c','agent','agent-a',7,'mention'),
                         ('root','c','agent','agent-x',6,'manual');"
                    )).await.unwrap();
                } else if setup == "following" {
                    db.execute(sqlx::raw_sql(
                        "INSERT INTO thread_followers(thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source)
                         VALUES('root','c','human','local',1,'manual');"
                    )).await.unwrap();
                }
            }
        }
        db.close().await.unwrap();
        let store = MessagingStore::open(path).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn idempotent_follow_acquires_the_writer_reservation_before_authority_reads() {
        let (dir, store) = fixture_store("following").await;
        let options = SqliteConnectOptions::new()
            .filename(dir.path().join("thread-follow.sqlite"))
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let mut blocker = SqliteConnection::connect_with(&options).await.unwrap();
        let blocker_tx = blocker.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let port = store.thread_follow();
        let pending = tokio::spawn(async move {
            port.follow(
                UserId::parse("local").unwrap(),
                ConversationId::parse("c").unwrap(),
                ThreadRootId::parse("root").unwrap(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(
            !pending.is_finished(),
            "idempotent follow read before acquiring the writer reservation"
        );
        blocker_tx.rollback().await.unwrap();
        let committed = pending.await.unwrap().unwrap().unwrap();
        assert!(
            !committed
                .bind(
                    &UserId::parse("local").unwrap(),
                    &ConversationId::parse("c").unwrap(),
                    &ThreadRootId::parse("root").unwrap(),
                )
                .unwrap()
                .changed()
        );
    }

    #[tokio::test]
    async fn absent_unfollow_acquires_the_writer_reservation_before_authority_reads() {
        let (dir, store) = fixture_store("normal").await;
        let options = SqliteConnectOptions::new()
            .filename(dir.path().join("thread-follow.sqlite"))
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let mut blocker = SqliteConnection::connect_with(&options).await.unwrap();
        let blocker_tx = blocker.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let port = store.thread_follow();
        let pending = tokio::spawn(async move {
            port.unfollow(
                UserId::parse("local").unwrap(),
                ConversationId::parse("c").unwrap(),
                ThreadRootId::parse("root").unwrap(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(
            !pending.is_finished(),
            "absent unfollow read before acquiring the writer reservation"
        );
        blocker_tx.rollback().await.unwrap();
        let committed = pending.await.unwrap().unwrap().unwrap();
        assert!(
            !committed
                .bind(
                    &UserId::parse("local").unwrap(),
                    &ConversationId::parse("c").unwrap(),
                    &ThreadRootId::parse("root").unwrap(),
                )
                .unwrap()
                .changed()
        );
    }

    #[tokio::test]
    async fn production_sqlite_and_router_consume_the_ts_signed_surface() {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/fixtures/thread-follow-wire.json"
        )))
        .unwrap();
        for case in fixture.as_array().unwrap() {
            let (_dir, store) = fixture_store(case["setup"].as_str().unwrap()).await;
            let method = match case["method"].as_str().unwrap() {
                "GET" => Method::GET,
                "POST" => Method::POST,
                "DELETE" => Method::DELETE,
                _ => unreachable!(),
            };
            let (status, body) = response(
                router(store.thread_follow(), RecordingAftermath::default()),
                method,
                case["path"].as_str().unwrap(),
                true,
            )
            .await;
            assert_eq!(
                status.as_u16(),
                case["status"].as_u64().unwrap() as u16,
                "{}",
                case["name"]
            );
            assert_eq!(body, case["body"], "{}", case["name"]);
        }
        assert_eq!(fixture.as_array().unwrap().len(), 8);
    }

    #[tokio::test]
    async fn deterministic_tie_order_and_changed_only_aftermath_are_separate_rust_gates() {
        let (_dir, store) = fixture_store("tie_order").await;
        let aftermath = RecordingAftermath::default();

        let (status, body) = response(
            router(store.thread_follow(), aftermath.clone()),
            Method::GET,
            "/api/channels/c/threads/reply/followers",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"thread_root_id":"root","viewer_following":false,"follower_agent_ids":["agent-a","agent-b"]})
        );

        for expected_changed in [true, false] {
            let (status, body) = response(
                router(store.thread_follow(), aftermath.clone()),
                Method::POST,
                "/api/channels/c/threads/reply/follow",
                true,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                body,
                json!({"thread_root_id":"root","changed":expected_changed,"viewer_following":true})
            );
        }
        assert_eq!(
            aftermath.calls.lock().unwrap().as_slice(),
            &[("local".into(), "c".into())]
        );
        for expected_changed in [true, false] {
            let (status, body) = response(
                router(store.thread_follow(), aftermath.clone()),
                Method::DELETE,
                "/api/channels/c/threads/root/follow",
                true,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                body,
                json!({"thread_root_id":"root","changed":expected_changed,"viewer_following":false})
            );
        }
        assert_eq!(aftermath.calls.lock().unwrap().len(), 2);
    }

    async fn assert_corrupt_existing_human_is_safe(method: Method) {
        let (dir, store) = fixture_store("following").await;
        let options = SqliteConnectOptions::new()
            .filename(dir.path().join("thread-follow.sqlite"))
            .foreign_keys(false);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute("PRAGMA ignore_check_constraints=ON")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE thread_followers SET source='corrupt' \
             WHERE thread_root_id='root' AND principal_kind='human' AND principal_id='local'",
        )
        .execute(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();

        let aftermath = RecordingAftermath::default();
        let (status, body) = response(
            router(store.thread_follow(), aftermath.clone()),
            method,
            "/api/channels/c/threads/root/follow",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error":"internal error"}));
        assert!(aftermath.calls.lock().unwrap().is_empty());

        let options = SqliteConnectOptions::new()
            .filename(dir.path().join("thread-follow.sqlite"))
            .foreign_keys(false);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        let source: String = sqlx::query_scalar(
            "SELECT source FROM thread_followers \
             WHERE thread_root_id='root' AND principal_kind='human' AND principal_id='local'",
        )
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(source, "corrupt");
    }

    #[tokio::test]
    async fn idempotent_follow_strictly_decodes_the_existing_human_row() {
        assert_corrupt_existing_human_is_safe(Method::POST).await;
    }

    #[tokio::test]
    async fn idempotent_unfollow_strictly_decodes_the_existing_human_row() {
        assert_corrupt_existing_human_is_safe(Method::DELETE).await;
    }

    async fn assert_stale_direct_pair_endpoint_is_corruption(method: Method) {
        let (_dir, store) = fixture_store("stale_direct_pair").await;
        let aftermath = RecordingAftermath::default();
        let (status, body) = response(
            router(store.thread_follow(), aftermath.clone()),
            method,
            "/api/channels/c/threads/root/follow",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error":"internal error"}));
        assert!(aftermath.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_direct_pair_human_endpoint_is_corruption_for_follow() {
        assert_stale_direct_pair_endpoint_is_corruption(Method::POST).await;
    }

    #[tokio::test]
    async fn stale_direct_pair_human_endpoint_is_corruption_for_unfollow() {
        assert_stale_direct_pair_endpoint_is_corruption(Method::DELETE).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refusal_order_and_post_commit_publication_failure_are_fail_closed_and_audited() {
        let (_dir, store) = fixture_store("normal").await;
        let failing = RecordingAftermath {
            calls: Arc::default(),
            fail: true,
        };
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_target(false)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let (status, body) = response(
            router(store.thread_follow(), failing.clone()),
            Method::POST,
            "/api/channels/c/threads/root/follow",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"thread_root_id":"root","changed":true,"viewer_following":true})
        );
        assert_eq!(failing.calls.lock().unwrap().len(), 1);
        let (status, body) = response(
            router(store.thread_follow(), RecordingAftermath::default()),
            Method::GET,
            "/api/channels/c/threads/root/followers",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["viewer_following"], true);
        let output = logs.text();
        let publication_errors = output
            .lines()
            .filter(|line| line.contains("committed thread-follow unread publication failed"))
            .collect::<Vec<_>>();
        assert_eq!(publication_errors.len(), 1);
        assert!(publication_errors[0].contains("event hub unavailable"));
        assert!(publication_errors[0].contains("scope event socket closed"));
        assert!(!body.to_string().contains("event hub unavailable"));
        assert!(!body.to_string().contains("scope event socket closed"));
        let (status, body) = response(
            router(store.thread_follow(), RecordingAftermath::default()),
            Method::GET,
            "/api/channels/c/threads/missing/followers",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            json!({"error":"thread root message not found in conversation","code":"reply_target_not_found"})
        );
        let (status, body) = response(
            router(store.thread_follow(), RecordingAftermath::default()),
            Method::GET,
            "/api/channels/missing/threads/root/followers",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"error":"channel not found"}));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn committed_unfollow_publication_failure_is_audited_without_compensation() {
        let (_dir, store) = fixture_store("following").await;
        let failing = RecordingAftermath {
            calls: Arc::default(),
            fail: true,
        };
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_target(false)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let (status, body) = response(
            router(store.thread_follow(), failing.clone()),
            Method::DELETE,
            "/api/channels/c/threads/root/follow",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"thread_root_id":"root","changed":true,"viewer_following":false})
        );
        assert_eq!(failing.calls.lock().unwrap().len(), 1);

        let (status, state) = response(
            router(store.thread_follow(), RecordingAftermath::default()),
            Method::GET,
            "/api/channels/c/threads/root/followers",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(state["viewer_following"], false);
        let output = logs.text();
        let publication_errors = output
            .lines()
            .filter(|line| line.contains("committed thread-unfollow unread publication failed"))
            .collect::<Vec<_>>();
        assert_eq!(publication_errors.len(), 1);
        assert!(publication_errors[0].contains("event hub unavailable"));
        assert!(publication_errors[0].contains("scope event socket closed"));
        assert!(!body.to_string().contains("event hub unavailable"));
        assert!(!body.to_string().contains("scope event socket closed"));
    }

    #[tokio::test]
    async fn every_selected_follower_is_strictly_decoded_before_filtering() {
        let (dir, store) = fixture_store("normal").await;
        let options = SqliteConnectOptions::new()
            .filename(dir.path().join("thread-follow.sqlite"))
            .foreign_keys(false);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query(
            "INSERT INTO thread_followers(thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source) \
             VALUES('root','c','agent','agent-a',1,'manual'),('root','c','agent','',2,'manual')",
        )
        .execute(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();
        let (status, body) = response(
            router(store.thread_follow(), RecordingAftermath::default()),
            Method::GET,
            "/api/channels/c/threads/root/followers",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error":"internal error"}));
    }
}
