use sqlx::Connection;

use crate::current_schema;

#[tokio::test]
async fn materialized_snapshot_enforces_foreign_keys_and_append_only_events() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("current.sqlite");
    let mut connection = current_schema::create_current_schema(&path).await;

    let foreign_key = sqlx::query(
        "INSERT INTO hosts
            (id, name, kind, created_at, user_id, provisioning_source)
         VALUES ('host-1', 'host-1', 'remote', 1, 'missing-user', 'user_invite')",
    )
    .execute(&mut connection)
    .await
    .unwrap_err();
    assert!(
        foreign_key
            .to_string()
            .contains("FOREIGN KEY constraint failed")
    );

    sqlx::query(
        "INSERT INTO agent_runtime_control_events
            (agent_id, generation, actor_user_id, reason, was_running, occurred_at)
         VALUES ('agent-1', 0, 'user-1', 'session_cleared', 0, 1)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    let trigger = sqlx::query("DELETE FROM agent_runtime_control_events")
        .execute(&mut connection)
        .await
        .unwrap_err();
    assert!(
        trigger
            .to_string()
            .contains("agent_runtime_control_events is append-only")
    );

    connection.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_a_dangling_symlink_before_opening_sqlite() {
    use std::os::unix::fs::symlink;

    let target_directory = tempfile::tempdir().unwrap();
    let sentinel_directory = tempfile::tempdir().unwrap();
    let database_path = target_directory.path().join("current.sqlite");
    let sentinel_path = sentinel_directory.path().join("sentinel.sqlite");
    symlink(&sentinel_path, &database_path).unwrap();

    let task_path = database_path.clone();
    let failure = tokio::spawn(async move {
        let _connection = current_schema::create_current_schema(&task_path).await;
    })
    .await
    .unwrap_err();
    assert!(failure.is_panic());
    assert!(std::fs::symlink_metadata(database_path).is_ok());
    assert!(!sentinel_path.exists());
}
