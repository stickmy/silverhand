use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde::Deserialize;
use serde_json::Value;
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_server::conversation_messaging::messaging::collaboration::human_search;
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
use tower::ServiceExt;

#[derive(Debug, Deserialize)]
struct FixtureEntry {
    name: String,
    query: String,
    status: u16,
    body: Value,
}

async fn seed(path: &std::path::Path) {
    const SCHEMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    ));
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
    for statement in [
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','host','local',1)",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('local','local','Local',1,1),('gone','gone','Gone',1,1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,display_name) \
         VALUES('pair-agent','pair-agent','codex','h','local',1,1,'Pair Agent')",
        "INSERT INTO workspaces(id,name,name_key,description,created_at,updated_at) \
         VALUES('w','cap-needle workspace','cap-needle-workspace','cap discovery',1,2)",
        "INSERT OR IGNORE INTO workspace_users(workspace_id,user_id,joined_at,role) \
         VALUES('w','local',1,'member')",
        "INSERT INTO conversations( \
         id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at \
         ) VALUES('c','w','workspace','open','Cap-Needle Channel','cap-needle-channel','idle','private',1,2)",
        "INSERT INTO conversations( \
         id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id, \
         title,status,visibility,created_at,updated_at \
         ) VALUES('pair','direct_pair','fixed','agent','pair-agent','human','local','','idle','private',1,1)",
        "INSERT OR IGNORE INTO conversation_users(conversation_id,user_id,joined_at) \
         VALUES('c','local',1)",
        "INSERT INTO messages(id,conversation_id,from_kind,content,seq,created_at) VALUES \
         ('cap-1','c','system','cap-needle one',1,10), \
         ('cap-2','c','system','cap-needle two',2,11), \
         ('cap-3','c','system','cap-needle three',3,12), \
         ('thread-root','c','system','thread root',4,20), \
         ('pair-message','pair','system','dm-needle',1,30)",
        "INSERT INTO messages(id,conversation_id,from_kind,content,seq,created_at,thread_root_id) \
         VALUES('thread-reply','c','system','thread-needle',5,21,'thread-root')",
        "INSERT INTO messages( \
         id,conversation_id,from_kind,from_user_id,content,seq,created_at,author_id,author_name,author_display_name \
         ) VALUES('deleted-message','c','user','gone','deleted-needle',6,22,'gone','gone','Gone')",
        "DELETE FROM users WHERE id='gone'",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn production_sqlite_router_consumes_the_typescript_wire_fixture() {
    let fixture: Vec<FixtureEntry> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/human-search-wire.json"
    )))
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("human-search-router.sqlite");
    seed(&path).await;
    let store = MessagingStore::open(&path).await.unwrap();
    let app = human_search::router(store.human_search());

    for entry in fixture {
        let mut request = Request::builder()
            .uri(format!("/api/search{}", entry.query))
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), entry.status, "{}", entry.name);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body, entry.body, "{}", entry.name);
    }
}

async fn search_json(app: &axum::Router, query: &str) -> (u16, Value) {
    let mut request = Request::builder()
        .uri(format!("/api/search{query}"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(UserId::parse("local").unwrap());
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn identity_search_is_one_snapshot_authorized_counted_and_scope_selected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identities.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    seed_identity_discovery(&mut db).await;
    let store = MessagingStore::open(&path).await.unwrap();
    let app = human_search::router(store.human_search());

    assert_identity_admission_and_budgets(&app).await;
    assert_identity_filter_states(&app).await;
    assert_identity_freshness(&app, &mut db).await;
    assert_identity_failure_and_content_scope(&app, &mut db).await;

    db.close().await.unwrap();
}

async fn seed_identity_discovery(db: &mut SqliteConnection) {
    for statement in [
        "UPDATE messages SET content='Discover message' WHERE id LIKE 'cap-%'",
        "UPDATE users SET display_name='Discover Self' WHERE id='local'",
        "INSERT INTO users(id,name,display_name,created_at,updated_at) \
         VALUES('outsider','outsider','Outsider',1,1)",
    ] {
        db.execute(statement).await.unwrap();
    }
    for i in 0..25 {
        let id = format!("discover-{i:02}");
        let display_name = format!("Discover {i:02}");
        sqlx::query(
            "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES(?,?,?,1,1)",
        )
        .bind(&id)
        .bind(&id)
        .bind(&display_name)
        .execute(&mut *db)
        .await
        .unwrap();
        seed_discovery_agent(
            db,
            &id,
            &format!("agent-{id}"),
            &display_name,
            "local",
            "active",
            "agent",
        )
        .await;
    }
    for (id, creator, stage, scope) in [
        ("hidden", "outsider", "active", "agent"),
        ("pending", "local", "pending_materialization", "agent"),
        ("channel", "local", "active", "conversation"),
        ("via-dm", "outsider", "active", "agent"),
        ("via-workspace", "outsider", "active", "agent"),
        ("via-space", "outsider", "active", "agent"),
    ] {
        seed_discovery_agent(db, id, id, &format!("Discover {id}"), creator, stage, scope).await;
    }
    for statement in [
        "INSERT INTO conversations(id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,\
         endpoint_b_kind,endpoint_b_id,title,status,visibility,created_at,updated_at) \
         VALUES('other-pair','direct_pair','fixed','agent','via-dm','human','local','','idle','private',1,1)",
        "INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('w','via-workspace',1)",
        "INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at) \
         VALUES('space','Space',NULL,'via-space',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('space','local',1,'member')",
    ] {
        db.execute(statement).await.unwrap();
    }
}

async fn seed_discovery_agent(
    db: &mut SqliteConnection,
    id: &str,
    name: &str,
    display_name: &str,
    creator: &str,
    stage: &str,
    scope: &str,
) {
    sqlx::query(
        "INSERT INTO agents(id,name,display_name,runtime,host_id,created_by_user_id,\
         materialization_status,session_scope,created_at,updated_at) \
         VALUES(?,?,?,'codex','h',?,?,?,1,1)",
    )
    .bind(id)
    .bind(name)
    .bind(display_name)
    .bind(creator)
    .bind(stage)
    .bind(scope)
    .execute(db)
    .await
    .unwrap();
}

async fn assert_identity_admission_and_budgets(app: &axum::Router) {
    // A one-row content budget cannot squeeze either identity category.
    let (status, body) = search_json(app, "?q=discover&include_identities=true&limit=1").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["identities"]["status"], "searched");
    assert_eq!(body["total"], 3);
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    for (kind, total) in [("humans", 25), ("agents", 28)] {
        let category = &body["identities"][kind];
        assert_eq!(category["total"], total);
        assert_eq!(category["results"].as_array().unwrap().len(), 20);
        assert_eq!(category["truncated"], true);
        assert_eq!(category["results"][0]["id"], "discover-00");
    }
    assert_eq!(body["identities"]["humans"]["results"][0]["kind"], "human");
    assert_eq!(body["identities"]["agents"]["results"][0]["kind"], "agent");
    for id in ["via-dm", "via-workspace", "via-space"] {
        let (_, found) = search_json(app, &format!("?q=%40{id}&include_identities=true")).await;
        assert_eq!(found["identities"]["agents"]["results"][0]["id"], id);
    }
    for id in ["hidden", "pending", "channel"] {
        let (_, hidden) = search_json(app, &format!("?q={id}&include_identities=true")).await;
        assert_eq!(hidden["identities"]["agents"]["total"], 0);
    }
}

async fn assert_identity_filter_states(app: &axum::Router) {
    for filter in [
        "workspace_id=w",
        "user_id=local",
        "agent_id=via-dm",
        "from=0",
        "to=10",
        "conversation_id=c",
    ] {
        let (_, filtered) = search_json(
            app,
            &format!("?q=discover&include_identities=true&{filter}"),
        )
        .await;
        assert_eq!(filtered["identities"]["status"], "filtered");
        assert_eq!(filtered["identities"]["humans"]["total"], 0);
        assert_eq!(filtered["identities"]["agents"]["total"], 0);
    }
    for text in ["", "%20", "%40"] {
        let (_, empty) = search_json(app, &format!("?q={text}&include_identities=true")).await;
        assert_eq!(empty["identities"]["status"], "empty_query");
    }
}

async fn assert_identity_freshness(app: &axum::Router, db: &mut SqliteConnection) {
    // Rename and ACL loss are visible on the next search, with no directory cache.
    db.execute(
        "UPDATE users SET name='renamed-human',display_name='Renamed Human' WHERE id='discover-00'",
    )
    .await
    .unwrap();
    db.execute("DELETE FROM workspace_members WHERE agent_id='via-workspace'")
        .await
        .unwrap();
    let (_, renamed) = search_json(app, "?q=renamed&include_identities=true").await;
    assert_eq!(
        renamed["identities"]["humans"]["results"][0]["id"],
        "discover-00"
    );
    let (_, revoked) = search_json(app, "?q=via-workspace&include_identities=true").await;
    assert_eq!(revoked["identities"]["agents"]["total"], 0);
}

async fn assert_identity_failure_and_content_scope(app: &axum::Router, db: &mut SqliteConnection) {
    // An invalid identity row must fail the selected read, not become zero hits.
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    db.execute("UPDATE agents SET runtime='invalid' WHERE id='discover-00'")
        .await
        .unwrap();
    assert_eq!(
        search_json(app, "?q=discover&include_identities=true")
            .await
            .0,
        500
    );
    let (status, legacy) = search_json(app, "?q=cap-needle").await;
    assert_eq!(status, 200);
    assert!(legacy.get("identities").is_none());
    assert!(legacy["query"].get("include_identities").is_none());
    assert_eq!(
        search_json(app, "?q=discover&include_identities=wrong")
            .await
            .0,
        400
    );
}

#[tokio::test]
async fn only_channel_search_hits_carry_archive_state_independently_of_closure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    let app = human_search::router(store.human_search());

    for (closed, archived) in [
        (None, None),
        (None, Some(30_i64)),
        (Some(20_i64), Some(30)),
        (Some(20), None),
    ] {
        sqlx::query("UPDATE conversations SET closed_at=?, archived_at=? WHERE id='c'")
            .bind(closed)
            .bind(archived)
            .execute(&mut db)
            .await
            .unwrap();
        let (status, body) = search_json(&app, "?q=cap-needle").await;
        assert_eq!(status, 200);
        let hits: Vec<_> = body["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|hit| hit["conversation"]["id"] == "c")
            .collect();
        assert!(hits.iter().any(|hit| hit["type"] == "conversation"));
        assert!(hits.iter().any(|hit| hit["type"] == "message"));
        for hit in hits {
            if hit["type"] == "conversation" {
                assert_eq!(
                    hit["conversation"]["archived_at"],
                    serde_json::json!(archived)
                );
            } else {
                assert!(hit["conversation"].get("archived_at").is_none());
            }
            assert_eq!(hit["conversation"]["closed_at"], serde_json::json!(closed));
        }
    }
    let (_, dm) = search_json(&app, "?q=dm-needle").await;
    assert!(
        dm["results"][0]["conversation"]
            .get("archived_at")
            .is_none()
    );
    db.close().await.unwrap();
}
