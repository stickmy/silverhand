//! Task history: by-id gate parity, task/subtype filtering, and metadata passthrough.

use silverhand_backend_core::conversation_messaging::messaging::task::{TaskHistoryRead, TaskId};
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_sqlite::conversation_messaging::messaging::task::SqliteTaskStore;
use sqlx::{Connection, SqliteConnection};

use crate::current_schema;

#[tokio::test]
async fn history_returns_only_this_tasks_lifecycle_events_behind_the_by_id_gate() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tasks.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in [
        "INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('user-1','user','User',1,1)",
        "INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('user-2','other','Other',1,1)",
        "INSERT INTO workspaces (id,name,name_key,created_at,updated_at) VALUES ('ws-1','Space','space',1,1)",
        "INSERT INTO workspace_users (workspace_id,user_id,role,joined_at) VALUES ('ws-1','user-1','member',1)",
        seed_channel!(
            id = "conv-1",
            workspace_id = "ws-1",
            title = "General",
            title_key = "general",
        ),
        "INSERT INTO conversation_users (conversation_id,user_id,joined_at) VALUES ('conv-1','user-1',1)",
        "INSERT INTO tasks (id,workspace_id,task_number,conversation_id,title,content,status,creator_kind,creator_user_id,created_at,board_key) VALUES ('task-1','ws-1',1,'conv-1','T','','todo','user','user-1',1,'ws:ws-1')",
        r#"INSERT INTO messages (id,conversation_id,from_kind,content,metadata,seq,created_at) VALUES
            ('e1','conv-1','system','c','{"subtype":"task_create","task_id":"task-1","task_number":1,"cross_conversation":true}',1,10),
            ('e2','conv-1','system','c','{"subtype":"task_status","task_id":"task-1","task_status":"done"}',2,20),
            ('o1','conv-1','system','c','{"subtype":"task_status","task_id":"task-2"}',3,30),
            ('o2','conv-1','system','c','{"subtype":"onboarding","task_id":"task-1"}',4,40),
            ('o3','conv-1','system','c','not json',5,50)"#,
        "INSERT INTO messages (id,conversation_id,from_kind,from_user_id,content,metadata,seq,created_at) VALUES ('o4','conv-1','user','user-1','c','{\"subtype\":\"task_edit\",\"task_id\":\"task-1\"}',6,60)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();

    let store = SqliteTaskStore::open(&path).await.unwrap();
    let user = |id: &str| UserId::parse(id).unwrap();
    let task = |id: &str| TaskId::parse(id).unwrap();

    let events = store
        .read_authorized_history(user("user-1"), task("task-1"))
        .await
        .unwrap()
        .expect("member reads history");
    let ids: Vec<_> = events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["e1", "e2"]);
    assert_eq!(events[0].created_at, 10);
    assert_eq!(
        events[0].metadata,
        serde_json::json!({"subtype":"task_create","task_id":"task-1","task_number":1,"cross_conversation":true})
    );

    assert!(
        store
            .read_authorized_history(user("user-2"), task("task-1"))
            .await
            .unwrap()
            .is_none(),
        "non-member gets the by-id not-found"
    );
    assert!(
        store
            .read_authorized_history(user("user-1"), task("missing"))
            .await
            .unwrap()
            .is_none()
    );

    let mut writer = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("DELETE FROM workspace_users WHERE workspace_id='ws-1' AND user_id='user-1'")
        .execute(&mut writer)
        .await
        .unwrap();
    writer.close().await.unwrap();
    assert!(
        store
            .read_authorized_history(user("user-1"), task("task-1"))
            .await
            .unwrap()
            .is_none(),
        "revocation closes history like the point read"
    );
}
