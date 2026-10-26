use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::channel_directory::{
        ActiveChannelDirectoryPort, ArchivedSince, ArchivedWorkspaceChannelDirectoryPort,
        GlobalArchivedChannelDirectoryPort, GlobalArchivedDirectoryRefusal,
    },
    foundation::identity::{UserId, WorkspaceId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection};

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('u','directory-viewer','Viewer',1,1),('other','directory-other','Other',1,1)",
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','Directory Host','local',1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) \
         VALUES('a','alice-directory','claude','h',1,1,'u')",
        "INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at) VALUES \
         ('w','Workspace','directory-workspace',NULL,1,1),\
         ('home','Alice Home',NULL,'a',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES \
         ('w','u',1),('home','u',1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,archived_at,created_at,updated_at) VALUES \
         ('public','w','workspace','open','Public','public-directory','active','public',NULL,1,1),\
         ('member','w','workspace','open','Member','member-directory','active','private',NULL,2,2),\
         ('hidden','w','workspace','open','Hidden','hidden-directory','active','private',NULL,3,3),\
         ('arch-public','w','workspace','open','Archived Public','archived-public-directory','archived','public',8,1,8),\
         ('arch','w','workspace','open','Archived Member','archived-member-directory','archived','private',9,2,9),\
         ('onboarding','home','workspace','fixed','Onboarding','onboarding-directory','active','private',NULL,3,3),\
         ('owner-dm','home','workspace','fixed','dm-alice-directory','owner-dm-directory','active','private',NULL,1,1),\
         ('other-dm','home','workspace','fixed','Other DM','other-dm-directory','active','private',NULL,2,2),\
         ('home-arch','home','workspace','fixed','Home Archived','home-arch-directory','archived','private',10,4,10)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES \
         ('member','u',1),('arch','u',1),('onboarding','u',1),('owner-dm','u',1),\
         ('other-dm','u',1),('home-arch','u',1),('arch','other',2)",
        "INSERT INTO user_list_sort_mode(user_id,list_key,scope,mode,updated_at) \
         VALUES('u','channels:w','','manual',1)",
        "INSERT INTO user_sort_entry(user_id,list_key,entity_id,rank,last_accessed_at,starred_at) VALUES \
         ('u','channels:w','public',2,3,NULL),('u','channels:w','member',1,4,5),\
         ('u','channels:w','hidden',0,9,NULL),\
         ('u','channels:w','stale-access',NULL,NULL,NULL),\
         ('u','channels:w','stale-star',NULL,NULL,NULL)",
    ] {
        db.execute(sql).await.unwrap();
    }
    db.close().await.unwrap();
}

fn ids(
    rows: &[silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_directory::ChannelDirectoryRow],
) -> Vec<&str> {
    rows.iter().map(|row| row.conversation().as_str()).collect()
}

#[tokio::test]
async fn workspace_faces_keep_public_widen_archives_and_agent_home_separate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-directory.sqlite");
    seed(&path).await;
    let read = MessagingStore::open(&path)
        .await
        .unwrap()
        .channel_directory();
    let viewer = UserId::parse("u").unwrap();

    let workspace = WorkspaceId::parse("w").unwrap();
    let active = read
        .active(&viewer, &workspace)
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &workspace)
        .unwrap();
    assert_eq!(ids(active.rows()), ["public", "member"]);
    assert_eq!(
        active
            .sort()
            .entries()
            .iter()
            .map(|entry| entry.conversation().as_str())
            .collect::<Vec<_>>(),
        ["member", "public"]
    );

    let archived = read
        .archived(&viewer, &workspace)
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &workspace)
        .unwrap();
    assert_eq!(ids(archived.rows()), ["arch"]);

    let home = WorkspaceId::parse("home").unwrap();
    let home_active = read
        .active(&viewer, &home)
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, &home)
        .unwrap();
    assert_eq!(
        ids(home_active.rows()),
        ["onboarding", "owner-dm", "other-dm"]
    );
}

#[tokio::test]
async fn global_archive_preserves_workspace_authority_and_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-directory-global.sqlite");
    seed(&path).await;
    let read = MessagingStore::open(&path)
        .await
        .unwrap()
        .channel_directory();
    let viewer = UserId::parse("u").unwrap();

    let all = read
        .archived_global(&viewer, None, None)
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, None)
        .unwrap();
    assert_eq!(
        all.rows()
            .iter()
            .map(|row| row.channel().conversation().as_str())
            .collect::<Vec<_>>(),
        ["home-arch", "arch"]
    );
    assert!(all.rows()[0].workspace_kind().agent_home_fact().is_some());
    assert!(all.rows()[1].workspace_kind().agent_home_fact().is_none());

    let missing = WorkspaceId::parse("missing").unwrap();
    let beyond = ArchivedSince::from_ecmascript_number(1e30).unwrap();
    assert_eq!(
        read.archived_global(&viewer, Some(&missing), Some(beyond))
            .await
            .unwrap(),
        Err(GlobalArchivedDirectoryRefusal::WorkspaceNotFound)
    );
    let workspace = WorkspaceId::parse("w").unwrap();
    let empty = read
        .archived_global(&viewer, Some(&workspace), Some(beyond))
        .await
        .unwrap()
        .unwrap()
        .bind(&viewer, Some(&workspace))
        .unwrap();
    assert!(empty.rows().is_empty());

    // The unfiltered aggregate follows the TS route's per-conversation seat
    // authority. Workspace membership is checked only when the caller asks for
    // an explicit workspace narrowing.
    let other = UserId::parse("other").unwrap();
    let other_all = read
        .archived_global(&other, None, None)
        .await
        .unwrap()
        .unwrap()
        .bind(&other, None)
        .unwrap();
    assert_eq!(
        other_all
            .rows()
            .iter()
            .map(|row| row.channel().conversation().as_str())
            .collect::<Vec<_>>(),
        ["arch"]
    );
    assert_eq!(
        read.archived_global(&other, Some(&workspace), None)
            .await
            .unwrap(),
        Err(GlobalArchivedDirectoryRefusal::Forbidden)
    );
}

#[tokio::test]
async fn selected_candidates_and_sort_state_decode_before_product_filtering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-directory-corrupt.sqlite");
    seed(&path).await;
    let viewer = UserId::parse("u").unwrap();
    let workspace = WorkspaceId::parse("w").unwrap();

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET status='unknown' WHERE id='hidden'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let read = MessagingStore::open(&path)
        .await
        .unwrap()
        .channel_directory();
    assert!(read.active(&viewer, &workspace).await.is_err());

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET status='active' WHERE id='hidden'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET latest_message_seq=-1 WHERE id='hidden'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read.active(&viewer, &workspace).await.is_err());

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE conversations SET latest_message_seq=0 WHERE id='hidden'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE user_sort_entry SET entity_id='' WHERE entity_id='hidden'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read.active(&viewer, &workspace).await.is_err());

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE user_sort_entry SET entity_id='hidden' WHERE entity_id=''")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read.active(&viewer, &workspace).await.unwrap().is_ok());

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE user_sort_entry SET rank='smuggled' WHERE entity_id='hidden'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read.active(&viewer, &workspace).await.is_err());

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE user_sort_entry SET rank=0 WHERE entity_id='hidden'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read.active(&viewer, &workspace).await.unwrap().is_ok());

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE user_sort_entry SET last_accessed_at='smuggled' WHERE entity_id='stale-access'",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    assert!(read.active(&viewer, &workspace).await.is_err());

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE user_sort_entry SET last_accessed_at=NULL WHERE entity_id='stale-access'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read.active(&viewer, &workspace).await.unwrap().is_ok());

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE user_sort_entry SET starred_at='smuggled' WHERE entity_id='stale-star'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read.active(&viewer, &workspace).await.is_err());

    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE user_sort_entry SET starred_at=NULL WHERE entity_id='stale-star'")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read.active(&viewer, &workspace).await.unwrap().is_ok());
}

#[tokio::test]
async fn global_kind_roster_mismatch_is_corruption_not_a_projectable_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-directory-kind.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    db.execute("DROP TRIGGER conversations_placement_immutable")
        .await
        .unwrap();
    sqlx::query(
        "UPDATE conversations SET roster_policy='fixed',visibility='private' WHERE id='arch'",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();

    let read = MessagingStore::open(&path)
        .await
        .unwrap()
        .channel_directory();
    assert!(
        read.archived_global(&UserId::parse("u").unwrap(), None, None)
            .await
            .is_err()
    );
}
