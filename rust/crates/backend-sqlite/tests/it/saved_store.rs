//! Store-layer gates for the saved linked-task projection. The HTTP router only
//! surfaces a generic 500 on corruption, so the *source-bearing* obligation — that a
//! malformed linked-task column names the exact column — is asserted here against the
//! typed `SavedStoreError`.

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::saved::{SavedListQuery, SavedReadPort},
    foundation::identity::UserId,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use silverhand_backend_sqlite::conversation_messaging::messaging::collaboration::SavedStoreError;
use sqlx::{Connection, Executor};

use crate::current_schema;

/// Seed a workspace channel with one saved message owned by `viewer`, but no task yet.
async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('viewer','viewer','Viewer',1,1)",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Workspace','workspace',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('w','viewer',1,'member')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,latest_message_seq,created_at,updated_at) VALUES \
         ('c','w','workspace','open','Channel','channel','idle','private',1,1,1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','viewer',1)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,\
         created_at,author_id,author_name,author_display_name) VALUES \
         ('m-1','c','user','viewer','saved one',1,10,'viewer','viewer','Viewer')",
        "INSERT INTO saved_messages(id,user_id,message_id,conversation_id,saved_at) VALUES \
         ('saved-1','viewer','m-1','c',1000)",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn insert_task(path: &std::path::Path, columns: &'static str) {
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute(columns).await.unwrap();
    db.close().await.unwrap();
}

/// Fault the saved list and return the column the typed source attributes the
/// corruption to. `PersistenceFailure` wraps the named `SavedStoreError`; the field
/// is `&'static str`, so it copies out past the borrow of the local error.
async fn faulting_field(path: &std::path::Path) -> &'static str {
    let error = MessagingStore::open(path)
        .await
        .unwrap()
        .saved()
        .list(
            &UserId::parse("viewer").unwrap(),
            SavedListQuery {
                workspace: None,
                since: None,
                sender: None,
            },
        )
        .await
        .expect_err("corrupt linked task must fault the saved list");
    let source = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<SavedStoreError>())
        .expect("saved failure preserves its named SQLite source");
    match source {
        SavedStoreError::MalformedLinkedTask { field } => field,
        other => panic!("expected MalformedLinkedTask, got {other:?}"),
    }
}

#[tokio::test]
async fn a_saved_linked_task_number_past_the_javascript_safe_integer_is_source_bearing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("saved-store-unsafe-number.sqlite");
    seed(&path).await;
    // Number.MAX_SAFE_INTEGER + 1 — tasks.task_number carries no upper CHECK.
    insert_task(
        &path,
        "INSERT INTO tasks(id,workspace_id,task_number,conversation_id,title,status,creator_kind,\
         creator_user_id,source_message_id,created_at,board_key) VALUES \
         ('task-1','w',9007199254740992,'c','Linked task','todo','user','viewer','m-1',1,'ws:w')",
    )
    .await;
    assert_eq!(faulting_field(&path).await, "tasks.task_number");
}

#[tokio::test]
async fn a_saved_non_canonical_linked_task_title_is_source_bearing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("saved-store-noncanonical-title.sqlite");
    seed(&path).await;
    insert_task(
        &path,
        "INSERT INTO tasks(id,workspace_id,task_number,conversation_id,title,status,creator_kind,\
         creator_user_id,source_message_id,created_at,board_key) VALUES \
         ('task-1','w',1,'c','  Linked   task  ','todo','user','viewer','m-1',1,'ws:w')",
    )
    .await;
    assert_eq!(faulting_field(&path).await, "tasks.title:non_canonical");
}
