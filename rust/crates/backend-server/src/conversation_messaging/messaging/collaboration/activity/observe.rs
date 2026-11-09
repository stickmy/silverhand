//! Authenticated HTTP adapter for `POST /api/attention/observe`.

use axum::{
    Json, Router,
    extract::{Extension, FromRequest, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::Deserialize;
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageSeq, NotificationId, ThreadRootId,
        collaboration::activity::observe::{
            ObservationAftermath, ObservationPort, ObserveBatch, ObserveRefusal, ObserveSource,
            ObservedSeq, StreamPosition, observe,
        },
    },
    foundation::identity::{ConversationId, UserId},
};
use std::sync::Arc;

use crate::foundation::wire_admission::ValidatedJson;
use crate::foundation::wire_patch::RequiredNullable;

pub const OBSERVE_PATH: &str = "/api/attention/observe";

/// `POST` body — the reference `z.object({ sources: z.array(ObserveSource).min(1).max(1000) })`.
/// Serde owns the discriminated-union shape; the `.min(1)/.max(1000)` array bound is a
/// stateless garde rule (a request-local semantic refusal → minimal 422). The per-source
/// identity/root/position legality stays in the core `observe` use case, gated per source
/// AFTER decode, so an unknown conversation refuses 404/403 rather than 422.
#[derive(Debug, Deserialize, Validate)]
struct ObserveBatchRaw {
    #[garde(length(min = 1, max = 1000), dive)]
    sources: Vec<ObserveSourceRaw>,
}

#[derive(Debug, Deserialize, Validate)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ObserveSourceRaw {
    Stream {
        #[garde(length(min = 1))]
        conversation_id: String,
        /// Required-and-nullable (`z.string().min(1).nullable()`): the key MUST be present,
        /// `null` = mainline, a non-empty id = that reply-cluster. `deserialize_present`
        /// (no `#[serde(default)]`) makes an ABSENT key a native `missing field` fault
        /// rather than silently mainline.
        #[serde(deserialize_with = "RequiredNullable::deserialize_present")]
        #[garde(skip)]
        thread_root_id: RequiredNullable<String>,
        #[garde(dive)]
        position: StreamPositionRaw,
    },
    Notification {
        #[garde(length(min = 1))]
        notification_id: String,
    },
}

#[derive(Debug, Deserialize, Validate)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StreamPositionRaw {
    AtSeq {
        /// `z.number().int().nonnegative()`. A non-integer or negative value is a native
        /// data fault; a nonneg integer at or beyond the SQLite i64 ceiling is still a
        /// valid ECMAScript integer and reaches the authority as `BeyondPersistedRange`
        /// (→ `position_future`), so the wire keeps it as `f64` and the domain mapping
        /// lives in `TryFrom` rather than being rejected structurally.
        #[garde(custom(nonnegative_integer))]
        seq: f64,
    },
    StreamHead,
}

fn nonnegative_integer(value: &f64, _: &()) -> garde::Result {
    // A finite value must be a non-negative integer. The `is_finite()` arm is a DEFENSIVE
    // type invariant only: in practice serde_json rejects a non-finite JSON literal
    // (`1e400` → out-of-range) at the `Json`/`ValidatedJson` decode step (native 400) before
    // this garde rule runs, so it never carries the primary rejection for those inputs — the
    // observe admission-only test proves the two overflow cells land on the native 400 axis,
    // not a garde 422. This arm exists so a +inf could never silently reach the domain if a
    // future decode path admitted one.
    if !value.is_finite() || value.fract() != 0.0 || *value < 0.0 {
        return Err(garde::Error::new(
            "seq must be a finite non-negative integer",
        ));
    }
    Ok(())
}

/// i64 exclusive ceiling — a nonneg integer at/above this is a valid ECMAScript integer
/// beyond SQLite's ordinal range; it reaches the authority as `BeyondPersistedRange`.
const I64_EXCLUSIVE_CEILING: f64 = 9_223_372_036_854_775_808.0;

impl TryFrom<StreamPositionRaw> for StreamPosition {
    type Error = ();
    fn try_from(raw: StreamPositionRaw) -> Result<Self, ()> {
        Ok(match raw {
            StreamPositionRaw::StreamHead => StreamPosition::StreamHead,
            StreamPositionRaw::AtSeq { seq } => {
                let observed = if seq >= I64_EXCLUSIVE_CEILING {
                    ObservedSeq::BeyondPersistedRange
                } else {
                    ObservedSeq::Persisted(MessageSeq::try_from(seq as i64).map_err(|_| ())?)
                };
                StreamPosition::AtSeq(observed)
            }
        })
    }
}

impl TryFrom<ObserveSourceRaw> for ObserveSource {
    type Error = ();
    fn try_from(raw: ObserveSourceRaw) -> Result<Self, ()> {
        Ok(match raw {
            ObserveSourceRaw::Notification { notification_id } => {
                ObserveSource::Notification(NotificationId::parse(notification_id).map_err(|_| ())?)
            }
            ObserveSourceRaw::Stream {
                conversation_id,
                thread_root_id,
                position,
            } => {
                let thread_root = match thread_root_id {
                    RequiredNullable::Null => None,
                    RequiredNullable::Value(id) if !id.is_empty() => {
                        Some(ThreadRootId::parse(id).map_err(|_| ())?)
                    }
                    // A present-but-empty string fails the `.min(1)` half — treated as a
                    // semantic refusal here (garde cannot dive through the newtype), so an
                    // empty thread_root_id is rejected rather than read as mainline.
                    RequiredNullable::Value(_) => return Err(()),
                };
                ObserveSource::Stream {
                    conversation: ConversationId::parse(conversation_id).map_err(|_| ())?,
                    thread_root,
                    position: StreamPosition::try_from(position)?,
                }
            }
        })
    }
}

impl TryFrom<ObserveBatchRaw> for ObserveBatch {
    type Error = ();
    fn try_from(raw: ObserveBatchRaw) -> Result<Self, ()> {
        let sources = raw
            .sources
            .into_iter()
            .map(ObserveSource::try_from)
            .collect::<Result<Vec<_>, ()>>()?;
        ObserveBatch::new(sources).map_err(|_| ())
    }
}

struct Inner<P, A> {
    port: P,
    aftermath: A,
}
struct ObserveState<P, A>(Arc<Inner<P, A>>);
impl<P, A> Clone for ObserveState<P, A> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P, A>(port: P, aftermath: A) -> Router
where
    P: ObservationPort + Send + Sync + 'static,
    A: ObservationAftermath + Send + Sync + 'static,
{
    Router::new()
        .route(OBSERVE_PATH, post(observe_route::<P, A>))
        .with_state(ObserveState(Arc::new(Inner { port, aftermath })))
}

async fn observe_route<P: ObservationPort, A: ObservationAftermath>(
    State(state): State<ObserveState<P, A>>,
    principal: Option<Extension<UserId>>,
    request: axum::extract::Request,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("Activity observe route reached without its human principal fact");
        return internal();
    };
    // Base gate is the human principal only; the body crosses the shared `ValidatedJson`
    // extractor next (structural faults keep native 415/400/422, the array bound is a garde
    // rule → minimal 422). Per-source identity/root/position legality is decided in the
    // core `observe` use case AFTER decode, so an unknown/forbidden source refuses 404/403
    // rather than 422 — hence the domain mapping stays a `TryFrom` yielding the minimal
    // semantic 422 only for a request-local shape it cannot represent.
    let raw = match ValidatedJson::<ObserveBatchRaw>::from_request(request, &()).await {
        Ok(ValidatedJson(raw)) => raw,
        Err(rejection) => return rejection.into_response(),
    };
    let batch = match ObserveBatch::try_from(raw) {
        Ok(batch) => batch,
        Err(()) => return crate::foundation::wire_admission::invalid_input(),
    };
    match observe(&state.0.port, &state.0.aftermath, viewer, batch).await {
        Ok(Ok(done)) => {
            if let Some(error) = done.first_failure() {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(error))
                    ),
                    "Activity observe effect failed"
                );
            }
            Json(json!({"changed":done.committed().changed_anything()})).into_response()
        }
        Ok(Err(refusal)) => refusal_response(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "Activity observe failed"
            );
            internal()
        }
    }
}

fn refusal_response(refusal: ObserveRefusal) -> Response {
    let (status, code, message) = match refusal {
        ObserveRefusal::ConversationNotFound => (
            StatusCode::NOT_FOUND,
            "conversation_not_found",
            "conversation not found",
        ),
        ObserveRefusal::Forbidden => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "cannot observe this conversation",
        ),
        ObserveRefusal::ThreadRootNotFound => (
            StatusCode::NOT_FOUND,
            "thread_root_not_found",
            "thread root message not found in this channel",
        ),
        ObserveRefusal::ThreadRootIsReply => (
            StatusCode::BAD_REQUEST,
            "thread_root_is_reply",
            "thread_root_id must be a root message, not a reply",
        ),
        ObserveRefusal::PositionFuture => (
            StatusCode::BAD_REQUEST,
            "position_future",
            "position seq is beyond the current stream head",
        ),
    };
    (status, Json(json!({"error":message,"code":code}))).into_response()
}

fn internal() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":"internal error"})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::activity::observe::{
        CommittedObservation, ObservationChanges, ObservationEffectFailure, ObservedConversation,
    };
    use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
    use sqlx::{Connection, Executor, Row, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::{
        io::{self, Write},
        sync::{Mutex, atomic::{AtomicUsize, Ordering}},
    };
    use tower::ServiceExt;
    use tracing_subscriber::fmt::MakeWriter;

    /// Wraps a real `ObservationPort` and counts how many times `observe` is invoked, so a
    /// test can prove the port itself is (or is not) reached — distinct from aftermath
    /// zero-reach. Delegates unchanged when reached, so a success positive control still
    /// commits through the real store.
    struct CountingObservationPort<P> {
        inner: P,
        calls: Arc<AtomicUsize>,
    }
    impl<P: ObservationPort + Send + Sync> ObservationPort for CountingObservationPort<P> {
        type Error = P::Error;
        async fn observe(
            &self,
            viewer: UserId,
            batch: ObserveBatch,
        ) -> Result<Result<ObservationChanges, ObserveRefusal>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.observe(viewer, batch).await
        }
    }

    #[derive(Clone, Default)]
    struct Effects(Arc<Mutex<Vec<&'static str>>>);
    impl ObservationAftermath for Effects {
        async fn conversation_changed(
            &self,
            _: &CommittedObservation,
            conversation: &ObservedConversation,
        ) -> Result<(), ObservationEffectFailure> {
            self.0
                .lock()
                .unwrap()
                .push(if conversation.workspace_id().is_some() {
                    "conversation"
                } else {
                    "pair"
                });
            Ok(())
        }
        async fn badge_changed(
            &self,
            _: &CommittedObservation,
            _: &silverhand_backend_core::foundation::identity::WorkspaceId,
        ) -> Result<(), ObservationEffectFailure> {
            self.0.lock().unwrap().push("badge");
            Ok(())
        }
        async fn notifications_dismissed(
            &self,
            _: &CommittedObservation,
        ) -> Result<(), ObservationEffectFailure> {
            self.0.lock().unwrap().push("dismiss");
            Ok(())
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("first aftermath cause")]
    struct FirstAftermathCause;

    #[derive(Debug, thiserror::Error)]
    #[error("later aftermath cause")]
    struct LaterAftermathCause;

    #[derive(Clone, Default)]
    struct FailingEffects(Arc<Mutex<Vec<&'static str>>>);

    impl ObservationAftermath for FailingEffects {
        async fn conversation_changed(
            &self,
            _: &CommittedObservation,
            _: &ObservedConversation,
        ) -> Result<(), ObservationEffectFailure> {
            self.0.lock().unwrap().push("conversation");
            Err(ObservationEffectFailure::from_source(FirstAftermathCause))
        }

        async fn badge_changed(
            &self,
            _: &CommittedObservation,
            _: &silverhand_backend_core::foundation::identity::WorkspaceId,
        ) -> Result<(), ObservationEffectFailure> {
            self.0.lock().unwrap().push("badge");
            Err(ObservationEffectFailure::from_source(LaterAftermathCause))
        }

        async fn notifications_dismissed(
            &self,
            _: &CommittedObservation,
        ) -> Result<(), ObservationEffectFailure> {
            self.0.lock().unwrap().push("dismiss");
            Err(ObservationEffectFailure::from_source(LaterAftermathCause))
        }
    }

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl LogBuffer {
        fn events(&self) -> Vec<Value> {
            let bytes = self.0.lock().unwrap().clone();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

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

    async fn fixture_store() -> (tempfile::TempDir, std::path::PathBuf, MessagingStore) {
        const SCHEMA: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity-observe-wire.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
        for sql in [
            "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('local','local-observe','Local',1,1),('other','other-observe','Other',1,1)",
            "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','W','w-observe',1,1),('w2','Forbidden','forbidden-observe',1,1)",
            "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','local',1)",
            seed_channel!(
                id = "c",
                workspace_id = "w",
                title = "C",
                title_key = "c-observe",
                latest_message_seq = 2,
            ),
            seed_channel!(
                id = "forbidden",
                workspace_id = "w2",
                roster_policy = "fixed",
                title = "Private",
                title_key = "private-observe",
            ),
            "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1)",
            "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,thread_root_id) VALUES('m1','c','user','other','one',1,1,NULL),('m2','c','user','other','two',2,2,NULL),('root','c','user','other','root',3,3,NULL),('reply','c','user','other','reply',4,4,'root')",
            "INSERT INTO conversation_stream_heads(conversation_id,thread_root_id,latest_seq) VALUES('c',NULL,2)",
            "INSERT INTO notifications(id,kind,user_id,workspace_id,conversation_id,message_id,payload,created_at) VALUES('n1','mention_self','local','w','c','m1','{}',1),('n2','mention_self','local','w','c','m2','{}',2)",
        ] {
            sqlx::query(sql).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        (dir, path, store)
    }

    async fn call(app: Router, body: Value, authenticated: bool) -> (StatusCode, Value) {
        call_raw(app, serde_json::to_string(&body).unwrap(), authenticated).await
    }

    #[tokio::test]
    async fn contended_observe_route_commits_seen_tail_and_leaves_later_unseen_message_unread() {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_unread::ConversationUnreadReadPort;
        use silverhand_backend_core::foundation::identity::ConversationId;
        let (_dir, path, store) = fixture_store().await;
        let effects = Effects::default();
        let app = router(store.observation(), effects.clone());
        let mut writer =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
                .await
                .unwrap();
        for sql in [
            "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES('m22','c','user','other','seen',22,22)",
            "UPDATE conversation_stream_heads SET latest_seq=22 WHERE conversation_id='c' AND thread_root_id IS NULL",
            "UPDATE conversations SET latest_message_seq=22 WHERE id='c'",
        ] {
            sqlx::query(sql).execute(&mut writer).await.unwrap();
        }
        let mut competing = writer.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("UPDATE conversations SET updated_at=23 WHERE id='c'")
            .execute(&mut *competing)
            .await
            .unwrap();
        let body = json!({"sources":[{"kind":"stream","conversation_id":"c","thread_root_id":null,"position":{"kind":"at_seq","seq":22}}]});
        let pending = tokio::spawn(call(app.clone(), body.clone(), true));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        competing.commit().await.unwrap();
        assert_eq!(
            pending.await.unwrap(),
            (StatusCode::OK, json!({"changed":true}))
        );
        let viewer = UserId::parse("local").unwrap();
        let conversation = ConversationId::parse("c").unwrap();
        let read = store.conversation_unread();
        assert_eq!(
            read.read(&viewer, &conversation)
                .await
                .unwrap()
                .unwrap()
                .bind(&conversation)
                .unwrap()
                .mainline()
                .count(),
            0
        );
        assert!(effects.0.lock().unwrap().contains(&"conversation"));

        for sql in [
            "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES('m23','c','user','other','unseen',23,23)",
            "UPDATE conversation_stream_heads SET latest_seq=23 WHERE conversation_id='c' AND thread_root_id IS NULL",
            "UPDATE conversations SET latest_message_seq=23 WHERE id='c'",
        ] {
            sqlx::query(sql).execute(&mut writer).await.unwrap();
        }
        assert_eq!(
            call(app, body, true).await,
            (StatusCode::OK, json!({"changed":false}))
        );
        assert_eq!(
            read.read(&viewer, &conversation)
                .await
                .unwrap()
                .unwrap()
                .bind(&conversation)
                .unwrap()
                .mainline()
                .count(),
            1
        );
        let cursor: i64 = sqlx::query_scalar("SELECT last_read_seq FROM conversation_reads WHERE user_id='local' AND conversation_id='c' AND thread_root_id IS NULL")
            .fetch_one(&mut writer).await.unwrap();
        assert_eq!(cursor, 22);
        writer.close().await.unwrap();
    }

    async fn call_raw(app: Router, body: String, authenticated: bool) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri(OBSERVE_PATH)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        if authenticated {
            request
                .extensions_mut()
                .insert(UserId::parse("local").unwrap());
        }
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        // A native axum structural rejection (415/400/422) carries a plain-text body pinned
        // on status only; a JSON body (success or minimal semantic 422) parses.
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn real_router_commits_once_then_reports_idempotent_and_postcommit_effects() {
        let (_dir, path, store) = fixture_store().await;
        let effects = Effects::default();
        let app = router(store.observation(), effects.clone());
        let body = json!({"sources":[
            {"kind":"stream","conversation_id":"c","thread_root_id":null,
             "position":{"kind":"at_seq","seq":1}},
            {"kind":"notification","notification_id":"n2"}
        ]});
        let first = call(app.clone(), body.clone(), true).await;
        assert_eq!(first, (StatusCode::OK, json!({"changed":true})));
        assert_eq!(*effects.0.lock().unwrap(), ["conversation", "dismiss"]);

        effects.0.lock().unwrap().clear();
        let second = call(app, body, true).await;
        assert_eq!(second, (StatusCode::OK, json!({"changed":false})));
        assert!(effects.0.lock().unwrap().is_empty());

        let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let cursor: i64 = sqlx::query_scalar("SELECT last_read_seq FROM conversation_reads WHERE user_id='local' AND conversation_id='c' AND thread_root_id IS NULL")
            .fetch_one(&mut db).await.unwrap();
        assert_eq!(cursor, 1);
        assert!(
            sqlx::query("SELECT read_at FROM notifications WHERE id='n1'")
                .fetch_one(&mut db)
                .await
                .unwrap()
                .get::<Option<i64>, _>("read_at")
                .is_some()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aftermath_failures_preserve_commit_and_report_only_the_first_cause() {
        let (_dir, path, store) = fixture_store().await;
        let effects = FailingEffects::default();
        let app = router(store.observation(), effects.clone());
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::ERROR)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let (status, body) = call(
            app,
            json!({"sources":[
                {"kind":"stream","conversation_id":"c","thread_root_id":null,
                 "position":{"kind":"at_seq","seq":1}},
                {"kind":"notification","notification_id":"n2"}
            ]}),
            true,
        )
        .await;
        assert_eq!((status, &body), (StatusCode::OK, &json!({"changed":true})));
        assert_eq!(*effects.0.lock().unwrap(), ["conversation", "dismiss"]);

        let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let cursor: i64 = sqlx::query_scalar("SELECT last_read_seq FROM conversation_reads WHERE user_id='local' AND conversation_id='c' AND thread_root_id IS NULL")
            .fetch_one(&mut db).await.unwrap();
        assert_eq!(cursor, 1);
        let read_at: Option<i64> =
            sqlx::query_scalar("SELECT read_at FROM notifications WHERE id='n2'")
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert!(read_at.is_some());

        let errors = logs.events();
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0]["fields"]["message"],
            "Activity observe effect failed"
        );
        let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
        assert!(chain.contains("a committed observation effect failed"));
        assert!(chain.contains("first aftermath cause"));
        assert!(!chain.contains("later aftermath cause"));
    }

    #[tokio::test]
    async fn missing_principal_precedes_invalid_body_without_reaching_observation() {
        let (_dir, _path, store) = fixture_store().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let effects = Effects::default();
        let app = router(
            CountingObservationPort {
                inner: store.observation(),
                calls: calls.clone(),
            },
            effects.clone(),
        );
        let missing = call(app, json!({"sources":[]}), false).await;
        assert_eq!(
            missing,
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"internal error"})
            )
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(effects.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn production_router_consumes_the_ts_signed_observe_wire() {
        // Denominator: the shared `activity-observe-wire.json` has 19 cells; this Rust
        // replay consumes the **11/19** that are genuine cross-language business/wire
        // contracts. The other **8/19** are marked `ts_authority="admission_only"` — pure
        // Zod-schema admission cells (missing/empty/wrong-typed sources, empty ids,
        // fractional/negative/overflow seq) whose TS signer emits the legacy 400{issues};
        // the Rust terminal admission is the shared native structural axis (400/422), so
        // those are NOT dual-signed here and are covered by the focused local
        // `all_eight_admission_only_cells_refuse_on_the_native_axis_before_the_observation_port`
        // test. The fixture was signed before TS retirement; no live TS signer runs.
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/fixtures/activity-observe-wire.json"
        )))
        .unwrap();
        let mut consumed = 0;
        for case in fixture.as_array().unwrap() {
            if case.get("ts_authority").and_then(Value::as_str) == Some("admission_only") {
                continue;
            }
            consumed += 1;
            let (_dir, path, store) = fixture_store().await;
            let app = router(store.observation(), Effects::default());
            let raw = case["request_json"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| serde_json::to_string(&case["request"]).unwrap());
            let (status, body) = call_raw(app, raw, true).await;
            assert_eq!(
                status.as_u16(),
                case["status"].as_u64().unwrap() as u16,
                "{}",
                case["name"]
            );
            assert_eq!(body, case["body"], "{}", case["name"]);
            if case["name"] == "position_future" {
                let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
                    .await
                    .unwrap();
                let count: i64 = sqlx::query_scalar("SELECT count(*) FROM conversation_reads")
                    .fetch_one(&mut db)
                    .await
                    .unwrap();
                assert_eq!(count, 0, "future refusal must not advance a cursor");
            }
        }
        assert_eq!(fixture.as_array().unwrap().len(), 19);
        assert_eq!(consumed, 11, "shared cross-language denominator is 11/19");
    }

    #[tokio::test]
    async fn all_eight_admission_only_cells_refuse_on_the_native_axis_before_the_observation_port()
    {
        // Mechanically cover EXACTLY the 8/19 `ts_authority="admission_only"` cells of the
        // shared fixture. Each is a body-admission fault that refuses on the native axis with
        // an EXACT per-cell status (six data-shape/stateless → 422; the two non-finite
        // `1e400`/`-1e400` seq literals → native 400, rejected by serde at the `Json` decode
        // step, NOT the garde `is_finite` arm). Crucially the request never reaches the
        // `ObservationPort` (proven by a counting port whose counter stays 0) NOR the
        // aftermath (Effects empty). A success positive control below proves the counter
        // does increment when a well-formed batch is admitted.
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-fixtures/fixtures/activity-observe-wire.json"
        )))
        .unwrap();
        let mut checked = 0;
        for case in fixture.as_array().unwrap() {
            if case.get("ts_authority").and_then(Value::as_str) != Some("admission_only") {
                continue;
            }
            checked += 1;
            let (_dir, _path, store) = fixture_store().await;
            let effects = Effects::default();
            let calls = Arc::new(AtomicUsize::new(0));
            let port = CountingObservationPort {
                inner: store.observation(),
                calls: calls.clone(),
            };
            let app = router(port, effects.clone());
            let raw = case["request_json"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| serde_json::to_string(&case["request"]).unwrap());
            let (status, body) = call_raw(app, raw, true).await;
            let name = case["name"].as_str().unwrap();
            let expected = match name {
                "positive_json_number_overflow_is_rejected"
                | "negative_json_number_overflow_is_rejected" => StatusCode::BAD_REQUEST,
                "missing_sources"
                | "empty_sources"
                | "empty_notification_id"
                | "wrong_sources_type"
                | "fractional_seq_is_rejected"
                | "negative_fraction_is_rejected" => StatusCode::UNPROCESSABLE_ENTITY,
                other => panic!("unexpected admission_only cell {other}"),
            };
            assert_eq!(status, expected, "{name}");
            if name == "empty_sources" {
                assert_eq!(body, json!({"error":"invalid input"}));
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "{name}: ObservationPort must be zero-reach"
            );
            assert!(
                effects.0.lock().unwrap().is_empty(),
                "{name}: observation aftermath must be zero-reach"
            );
        }
        assert_eq!(checked, 8, "admission-only local denominator is 8/19");

        // Positive control: a well-formed batch is admitted and DOES reach the counting
        // ObservationPort exactly once — so the zero-reach assertions above are meaningful,
        // not vacuously true because the port is never called.
        let (_dir, _path, store) = fixture_store().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let port = CountingObservationPort {
            inner: store.observation(),
            calls: calls.clone(),
        };
        let app = router(port, Effects::default());
        let raw = r#"{"sources":[{"kind":"notification","notification_id":"n2"}]}"#.to_owned();
        let (status, _) = call_raw(app, raw, true).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a well-formed batch must reach the ObservationPort exactly once"
        );
    }
}
