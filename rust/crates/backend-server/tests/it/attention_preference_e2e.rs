use crate::current_schema;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request},
};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        EnvironmentUnavailable, SendClock, collaboration::attention_preference::*,
    },
    foundation::identity::UserId,
};
use silverhand_backend_server::conversation_messaging::messaging::collaboration::attention_preference;
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::Connection;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

#[derive(Clone)]
struct FailingClock(Arc<AtomicUsize>);
impl SendClock for FailingClock {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(EnvironmentUnavailable::from_source(io::Error::other(
            "clock",
        )))
    }
}
#[derive(Clone, Copy)]
struct Effects;
impl AttentionPreferenceAftermath for Effects {
    async fn channel_preference_changed(
        &self,
        _: &impl ChannelAttentionCommit,
    ) -> Result<(), AttentionPreferenceEffectFailure> {
        Ok(())
    }
    async fn channel_badge_changed(
        &self,
        _: &impl ChannelAttentionCommit,
    ) -> Result<(), AttentionPreferenceEffectFailure> {
        Ok(())
    }
    async fn workspace_preference_changed(
        &self,
        _: &impl WorkspaceAttentionCommit,
    ) -> Result<(), AttentionPreferenceEffectFailure> {
        Ok(())
    }
    async fn workspace_badge_changed(
        &self,
        _: &impl WorkspaceAttentionCommit,
    ) -> Result<(), AttentionPreferenceEffectFailure> {
        Ok(())
    }
}
async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u','u','U',1,1)",
        "INSERT INTO hosts(id,name,kind,created_at,user_id) VALUES('h','H','local',1,'u')",
        "INSERT INTO agents(id,name,runtime,host_id,display_name,created_by_user_id,created_at,updated_at,materialization_status,session_scope) VALUES('a','a','codex','h','A','u',1,1,'active','agent')",
        "INSERT INTO workspaces(id,name,name_key,created_at,owner_user_id,updated_at) VALUES('w','W','w',1,'u',1)",
        "INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,owner_user_id,updated_at) VALUES('home','Home',NULL,'a',1,'u',1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','u',1),('home','u',1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at) VALUES('c','w','workspace','open','C','c','open',1,1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','u',1)",
    ] {
        sqlx::query(sql).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}
async fn call(app: Router, method: Method, path: &str, body: &str) -> (u16, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    request.extensions_mut().insert(UserId::parse("u").unwrap());
    let response = app.oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    // A native structural rejection (415/400/422) carries a plain-text body pinned on
    // status only; JSON bodies (our own {error} refusals / success) parse.
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn failing_clock_cannot_preempt_target_access_kind_or_body_refusals() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.sqlite");
    seed(&path).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let store = MessagingStore::open_with(&path, FailingClock(calls.clone()))
        .await
        .unwrap()
        .attention_preferences();
    for (method, path, body, status, expected) in [
        (
            Method::PUT,
            "/api/conversations/missing/attention-preference",
            r#"{"mode":"loud"}"#,
            404,
            serde_json::json!({"error":"channel not found"}),
        ),
        (
            Method::PUT,
            "/api/workspaces/missing/attention-preference",
            r#"{"muted":"yes"}"#,
            403,
            serde_json::json!({"error":"forbidden"}),
        ),
        (
            Method::PUT,
            "/api/workspaces/home/attention-preference",
            r#"{"muted":"yes"}"#,
            404,
            serde_json::json!({"error":"workspace not found"}),
        ),
    ] {
        let (actual, wire) = call(
            attention_preference::router(store.clone(), Effects),
            method,
            path,
            body,
        )
        .await;
        assert_eq!(actual, status, "{path}");
        assert_eq!(wire, expected, "{path}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{path}");
    }
    for (path, body) in [
        (
            "/api/conversations/c/attention-preference",
            r#"{"mode":"loud"}"#,
        ),
        (
            "/api/workspaces/w/attention-preference",
            r#"{"muted":"yes"}"#,
        ),
    ] {
        let (status, _wire) = call(
            attention_preference::router(store.clone(), Effects),
            Method::PUT,
            path,
            body,
        )
        .await;
        // A wrong-typed body value that survives to the shared extractor after admit is a
        // native data-shape 422 (native body text not pinned); the failing clock is never
        // consulted (writer/effect zero-reach).
        assert_eq!(status, 422);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{path}");
    }
    let (status, wire) = call(
        attention_preference::router(store, Effects),
        Method::PUT,
        "/api/conversations/c/attention-preference",
        r#"{"mode":"all"}"#,
    )
    .await;
    assert_eq!(status, 500);
    assert_eq!(wire, serde_json::json!({"error":"internal error"}));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
