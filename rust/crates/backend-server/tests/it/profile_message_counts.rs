use crate::current_schema;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::Value;
use silverhand_backend_core::identity_access::auth::{
    HumanSessionAuthenticator, SessionClock, SessionTtl,
};
use silverhand_backend_server::{
    foundation::clock::ProcessClock,
    identity_access::{
        auth::{PublicJsonUnauthorized, require_human_session},
        profile_message_counts,
    },
};
use silverhand_backend_sqlite::{
    conversation_messaging::messaging::MessagingStore,
    identity_access::{
        agent_detail_read::SqliteAgentDetailColdRead, auth::SqliteSessionStore,
        user_identity::SqliteUserStore,
    },
};
use sqlx::{Connection, Executor};
use tower::ServiceExt;
#[derive(Clone)]
struct Clock;
impl ProcessClock for Clock {
    fn now_unix_millis(&self) -> i64 {
        1735660800000
    }
}
impl SessionClock for Clock {
    fn now_unix_millis(&self) -> i64 {
        10000
    }
}
async fn call(app: &Router, kind: &str, id: &str, token: bool) -> (u16, Value) {
    let mut r = Request::builder().uri(format!(
        "/api/profiles/{kind}/{id}/message-counts?year=2024"
    ));
    if token {
        r = r.header("authorization", "Bearer valid-token");
    }
    let r = app
        .clone()
        .oneshot(r.body(Body::empty()).unwrap())
        .await
        .unwrap();
    (
        r.status().as_u16(),
        serde_json::from_slice(&to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap(),
    )
}
#[tokio::test]
async fn actual_http_counts_private_sources_without_exposing_them_and_rechecks_subject_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counts.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u','user','User',1,1),('v','viewer','Viewer',1,1),('empty','empty','Empty',1,1);
    INSERT INTO auth_sessions(id,user_id,token_hash,created_at,last_used_at) VALUES('session','u','OXoqnFv14szsOMJZa2grsb0F_m5OzqbBDPQnVf8iVAM',1,9500);
    INSERT INTO hosts(id,name,kind,created_at) VALUES('h','host','local',1);
    INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at) VALUES('u','agent','codex','h','u',1,1);
    INSERT INTO conversations(id,placement_kind,roster_policy,title,status,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,created_at,updated_at) VALUES('private','direct_pair','fixed','','open','human','u','human','v',1,1);" )).await.unwrap();
    let start = 1704038400000i64; // Shanghai 2024-01-01.
    for (id, kind, offset, cc, thread) in [
        ("root", "user", 0, None, None),
        ("reply", "user", 1, None, Some("root")),
        ("agent", "agent", 2, None, None),
        ("system", "system", 3, None, None),
        ("cc", "user", 4, Some("root"), None),
        ("before", "user", -1, None, None),
        ("end", "user", 366 * 86400000, None, None),
        ("leap", "user", 59 * 86400000, None, None),
    ] {
        sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,from_agent_id,content,seq,created_at,cc_source_message_id,thread_root_id) VALUES(?,'private',?,'u',?,'private-body',?,?,?,?)")
        .bind(id).bind(kind).bind(if kind=="agent"{Some("u")}else{None}).bind(start+offset).bind(start+offset).bind(cc).bind(thread).execute(&mut db).await.unwrap();
    }
    let app = require_human_session(
        profile_message_counts::router(
            MessagingStore::open(&path).await.unwrap(),
            SqliteUserStore::open(&path).await.unwrap(),
            SqliteAgentDetailColdRead::open(&path).await.unwrap(),
            Clock,
        ),
        HumanSessionAuthenticator::new(
            SqliteSessionStore::open(&path).await.unwrap(),
            Clock,
            SessionTtl::from_millis(1000).unwrap(),
        ),
        PublicJsonUnauthorized,
    );
    assert_eq!(call(&app, "user", "u", false).await.0, 401);
    let (status, body) = call(&app, "user", "u", true).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["subject"],
        serde_json::json!({"kind":"user", "id":"u"})
    );
    assert_eq!(body["year"], 2024);
    assert_eq!(body["time_zone"], "Asia/Shanghai");
    assert_eq!(
        body["available_years"],
        serde_json::json!([2023, 2024, 2025])
    );
    assert_eq!(body["days"].as_array().unwrap().len(), 366);
    assert_eq!(body["days"][0]["count"], 2);
    assert_eq!(body["days"][59]["count"], 1);
    assert!(!body.to_string().contains("private"));
    assert_eq!(
        body["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["count"].as_i64().unwrap())
            .sum::<i64>(),
        3
    );
    assert_eq!(
        call(&app, "agent", "u", true).await.1["days"][0]["count"],
        1
    );
    assert_eq!(
        call(&app, "user", "empty", true).await.1["days"][0]["count"],
        0
    );
    assert_eq!(call(&app, "user", "absent", true).await.0, 404);
    sqlx::query("UPDATE messages SET content='edited' WHERE id='root'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(call(&app, "user", "u", true).await.1, body);
    sqlx::query("DELETE FROM messages WHERE id='leap'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        call(&app, "user", "u", true).await.1["days"][59]["count"],
        0
    );
    let remaining = call(&app, "user", "u", true).await.1;
    sqlx::query("UPDATE agents SET created_by_user_id='v' WHERE id='u'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(call(&app, "agent", "u", true).await.0, 404);
    sqlx::query("UPDATE auth_sessions SET user_id='empty'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(call(&app, "user", "u", true).await.1["days"][0]["count"], 2);
    assert_eq!(call(&app, "agent", "u", true).await.0, 404);
    assert_eq!(call(&app, "user", "u", true).await.1, remaining);
    sqlx::query("DELETE FROM messages WHERE id='root'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(call(&app, "user", "u", true).await.1["days"][0]["count"], 0);
    db.close().await.unwrap();
}
