use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_server::conversation_messaging::messaging::collaboration::attention_projection;
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::Connection;
use tower::ServiceExt;

use crate::current_schema;

async fn seed(path: &std::path::Path, setup: &str) {
    let mut db = current_schema::create_current_schema(path).await;
    if setup == "empty" {
        db.close().await.unwrap();
        return;
    }
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('local','local-projection','Local',1,1),('other','other-projection','Other',1,1)",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','W','w-projection',1,1)",
    ] {
        sqlx::query(sql).execute(&mut db).await.unwrap();
    }
    if setup == "tiered" || setup == "tiered_unmuted" {
        for sql in [
            "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','local',1)",
            "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at,latest_message_seq) VALUES('c-b','w','workspace','open','B','b-projection','open',2,2,1),('c-c','w','workspace','open','C','c-projection','open',1,1,0),('c-a','w','workspace','open','A','a-projection','open',1,1,1)",
            "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c-a','local',1),('c-b','local',1),('c-c','local',1)",
            "INSERT INTO channel_attention_preferences(user_id,conversation_id,mode,updated_at) VALUES('local','c-a','mentions',1),('local','c-c','mute',1)",
            "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,author_id,author_name,author_display_name) VALUES('m-a','c-a','user','other','mention',1,1,'other','other-projection','Other'),('m-b','c-b','user','other','plain',1,1,'other','other-projection','Other')",
            "INSERT INTO notifications(id,kind,user_id,workspace_id,conversation_id,message_id,payload,created_at) VALUES('n-1','mention_self','local','w','c-a','m-a','{}',1),('n-2','mention_self','local','w','c-a','m-a','{}',2)",
        ] {
            sqlx::query(sql).execute(&mut db).await.unwrap();
        }
        sqlx::query("INSERT INTO workspace_attention_preferences(user_id,workspace_id,muted,updated_at) VALUES('local','w',?,1)")
            .bind(if setup == "tiered" { 1_i64 } else { 0_i64 })
            .execute(&mut db)
            .await
            .unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn real_sqlite_and_production_router_consume_the_ts_signed_fixture() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/attention-projection-wire.json"
    )))
    .unwrap();
    let mut surfaces = std::collections::BTreeSet::new();
    for entry in fixture.as_array().unwrap() {
        surfaces.insert(entry["surface"].as_str().unwrap());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("projection.sqlite");
        seed(&path, entry["setup"].as_str().unwrap()).await;
        let projection = MessagingStore::open(&path)
            .await
            .unwrap()
            .attention_projection();
        let mut request = Request::builder()
            .uri(entry["path"].as_str().unwrap())
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = attention_projection::router(projection)
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            entry["status"].as_u64().unwrap() as u16,
            "{}",
            entry["name"]
        );
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body, entry["body"], "{}", entry["name"]);
    }
    assert_eq!(
        surfaces,
        std::collections::BTreeSet::from(["badges", "modes"])
    );
    assert_eq!(fixture.as_array().unwrap().len(), 11);
}
