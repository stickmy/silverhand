use silverhand_backend_core::conversation_messaging::messaging::task::{
    HumanTaskBoardRead, TaskBoardColumnRequest, TaskBoardFilters, TaskBoardPerson, TaskStatus,
};
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_sqlite::conversation_messaging::messaging::task::SqliteTaskStore;
use sqlx::Connection;

use crate::current_schema;

fn filters() -> TaskBoardFilters {
    TaskBoardFilters {
        viewer: UserId::parse("user-1").unwrap(),
        workspace: None,
        conversation: None,
        container: None,
        assignee: None,
        creator: None,
    }
}

#[tokio::test]
async fn viewer_gate_precedes_window_counts_and_facets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tasks.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in [
        "INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('user-1','u1','Zoe',1,1),('user-2','u2','Amy',1,1)",
        "INSERT INTO workspaces (id,name,name_key,created_at,updated_at) VALUES ('ws-1','One','one',1,1),('ws-2','Two','two',1,1)",
        "INSERT INTO workspace_users (workspace_id,user_id,role,joined_at) VALUES ('ws-1','user-1','member',1),('ws-2','user-2','member',1)",
        "INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at) VALUES ('conv-1','ws-1','workspace','open','General','general','active',1,1),('conv-2','ws-2','workspace','open','Hidden','hidden','active',1,1)",
        "INSERT INTO conversation_users (conversation_id,user_id,joined_at) VALUES ('conv-1','user-1',1),('conv-2','user-2',1)",
        "INSERT INTO tasks (id,workspace_id,task_number,conversation_id,title,status,creator_kind,creator_user_id,assignee_user_id,created_at,board_key) VALUES ('visible-2','ws-1',2,'conv-1','Visible two','todo','user','user-1','user-1',2,'ws:ws-1'),('visible-1','ws-1',1,'conv-1','Visible one','todo','user','user-1',NULL,1,'ws:ws-1'),('hidden-99','ws-2',99,'conv-2','Hidden','todo','user','user-2','user-2',99,'ws:ws-2')",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let store = SqliteTaskStore::open(&path).await.unwrap();

    let page = store
        .column(&TaskBoardColumnRequest {
            filters: filters(),
            status: TaskStatus::Todo,
            cursor: None,
            limit: 1,
        })
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].task().id().as_str(), "visible-2");
    assert!(
        page.has_more,
        "the extra visible row, not the hidden row, drives the cursor"
    );

    let counts = store.counts(&filters()).await.unwrap();
    assert_eq!(counts.todo.get(), 2);
    assert_eq!(counts.done.get(), 0);

    let facets = store
        .facets(&UserId::parse("user-1").unwrap(), None)
        .await
        .unwrap();
    assert_eq!(facets.assignees.len(), 1);
    assert!(
        matches!(&facets.assignees[0], TaskBoardPerson::User { id, .. } if id.as_str() == "user-1")
    );
    assert!(facets.has_unassigned);
    assert_eq!(
        facets.creators,
        vec![TaskBoardPerson::User {
            id: UserId::parse("user-1").unwrap(),
            name: "Zoe".into(),
        }]
    );

    // A later request starts from the revoked population and cannot retain a
    // private person from the earlier statement snapshot.
    let mut db = sqlx::SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("DELETE FROM workspace_users WHERE workspace_id='ws-1' AND user_id='user-1'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let revoked = store
        .facets(&UserId::parse("user-1").unwrap(), None)
        .await
        .unwrap();
    assert!(revoked.assignees.is_empty());
    assert!(revoked.creators.is_empty());
    assert!(!revoked.has_unassigned);
}
