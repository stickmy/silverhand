use std::{error::Error, time::Duration};

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AttachmentId,
        collaboration::attachment::{
            AttachmentContentType, AttachmentContentUpdateCommand, AttachmentContentUpdateRefusal,
            AttachmentContentUpdateUnitOfWork, AttachmentCreateCommand, AttachmentCreateRefusal,
            AttachmentCreateUnitOfWork, AttachmentDeleteCommand, AttachmentDeleteRefusal,
            AttachmentDeleteUnitOfWork, AttachmentFilename,
        },
    },
    foundation::blob_store::{BlobStorageKey, StorageKind, StoredBlob},
    foundation::identity::{ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
    foundation::wire_number::JsSafeInteger,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::attachment::AttachmentWriteStoreError,
};
use sqlx::{Connection, Executor, Row, SqliteConnection};

use crate::current_schema;

fn user(raw: &str) -> UserId {
    UserId::parse(raw).unwrap()
}

fn conversation(raw: &str) -> ConversationId {
    ConversationId::parse(raw).unwrap()
}

fn attachment(raw: &str) -> AttachmentId {
    AttachmentId::parse(raw).unwrap()
}

fn blob(key: &str) -> StoredBlob {
    StoredBlob::new(StorageKind::Local, BlobStorageKey::parse(key).unwrap())
}

fn create_command(id: &str, viewer: &str, key: &str) -> AttachmentCreateCommand {
    AttachmentCreateCommand::new(
        user(viewer),
        conversation("c"),
        attachment(id),
        AttachmentFilename::parse("mockup.png").unwrap(),
        4,
        AttachmentContentType::parse("image/png").unwrap(),
        blob(key),
        10,
    )
    .unwrap()
}

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('owner','attachment-owner','Owner',1,1),('other','attachment-other','Other',1,1)",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Attachment Workspace','attachment-workspace',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('w','owner',1,'member'),('w','other',1,'member')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,created_at,updated_at) VALUES \
         ('c','w','workspace','open','General','general','idle','private',1,1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES \
         ('c','owner',1)",
    ] {
        db.execute(sql).await.unwrap();
    }
    db.close().await.unwrap();
}

fn write_source(error: &PersistenceFailure) -> &AttachmentWriteStoreError {
    Error::source(error)
        .and_then(|source| source.downcast_ref())
        .expect("write failures preserve their named SQLite source")
}

#[tokio::test]
async fn create_reserves_membership_and_returns_the_exact_committed_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attachment-create.sqlite");
    seed(&path).await;
    let writes = MessagingStore::open(&path)
        .await
        .unwrap()
        .attachment_writes();

    assert_eq!(
        writes
            .create(create_command(
                "forbidden",
                "other",
                "attachments/c/forbidden.png"
            ))
            .await
            .unwrap(),
        Err(AttachmentCreateRefusal::Forbidden)
    );
    let command = create_command("a", "owner", "attachments/c/a.png");
    let fact = writes.create(command).await.unwrap().unwrap();
    let bound = fact.bind(&user("owner"), &conversation("c")).unwrap();
    assert_eq!(bound.record().id().as_str(), "a");
    assert_eq!(bound.record().blob().key().as_str(), "attachments/c/a.png");

    assert_eq!(
        writes
            .create(
                AttachmentCreateCommand::new(
                    user("owner"),
                    conversation("missing"),
                    attachment("missing-conversation"),
                    AttachmentFilename::parse("a.png").unwrap(),
                    4,
                    AttachmentContentType::parse("image/png").unwrap(),
                    blob("attachments/missing/a.png"),
                    10,
                )
                .unwrap()
            )
            .await
            .unwrap(),
        Err(AttachmentCreateRefusal::NotFound)
    );
}

#[tokio::test]
async fn create_reads_authority_only_after_its_writer_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attachment-create-reservation.sqlite");
    seed(&path).await;
    let writes = MessagingStore::open(&path)
        .await
        .unwrap()
        .attachment_writes();
    let mut blocker = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    blocker.execute("BEGIN IMMEDIATE").await.unwrap();
    let pending = {
        let writes = writes.clone();
        tokio::spawn(async move {
            writes
                .create(create_command(
                    "late-create",
                    "owner",
                    "attachments/c/late-create.png",
                ))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(!pending.is_finished());
    blocker
        .execute("DELETE FROM conversation_users WHERE conversation_id='c' AND user_id='owner'")
        .await
        .unwrap();
    blocker.execute("COMMIT").await.unwrap();

    assert_eq!(
        pending.await.unwrap().unwrap(),
        Err(AttachmentCreateRefusal::Forbidden)
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE id='late-create'")
        .fetch_one(&mut blocker)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn update_authority_is_reread_after_writer_reservation_then_seals_pointer_swap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attachment-update.sqlite");
    seed(&path).await;
    let writes = MessagingStore::open(&path)
        .await
        .unwrap()
        .attachment_writes();
    writes
        .create(create_command("a", "owner", "attachments/c/old.png"))
        .await
        .unwrap()
        .unwrap();

    let mut blocker = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    blocker.execute("BEGIN IMMEDIATE").await.unwrap();
    let pending = {
        let writes = writes.clone();
        tokio::spawn(async move {
            writes
                .update_content(
                    AttachmentContentUpdateCommand::new(
                        user("owner"),
                        attachment("a"),
                        AttachmentContentType::parse("image/png").unwrap(),
                        8,
                        blob("attachments/c/denied.png"),
                    )
                    .unwrap(),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(!pending.is_finished());
    blocker
        .execute("DELETE FROM conversation_users WHERE conversation_id='c' AND user_id='owner'")
        .await
        .unwrap();
    blocker.execute("COMMIT").await.unwrap();
    assert_eq!(
        pending.await.unwrap().unwrap(),
        Err(AttachmentContentUpdateRefusal::Forbidden)
    );

    sqlx::query(
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','owner',2)",
    )
    .execute(&mut blocker)
    .await
    .unwrap();
    let updated = writes
        .update_content(
            AttachmentContentUpdateCommand::new(
                user("owner"),
                attachment("a"),
                AttachmentContentType::parse("image/png").unwrap(),
                8,
                blob("attachments/c/new.png"),
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .unwrap()
        .bind(&user("owner"), &attachment("a"))
        .unwrap();
    assert_eq!(
        updated.before().blob().key().as_str(),
        "attachments/c/old.png"
    );
    assert_eq!(
        updated.after().blob().key().as_str(),
        "attachments/c/new.png"
    );
    assert_eq!(updated.after().content_revision().get(), 2);
    assert_eq!(updated.changed().attachment().as_str(), "a");
}

#[tokio::test]
async fn revision_ceiling_is_rejected_before_update_and_repair_uses_same_port() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attachment-revision.sqlite");
    seed(&path).await;
    let writes = MessagingStore::open(&path)
        .await
        .unwrap()
        .attachment_writes();
    writes
        .create(create_command("a", "owner", "attachments/c/old.png"))
        .await
        .unwrap()
        .unwrap();
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("UPDATE attachments SET content_revision=? WHERE id='a'")
        .bind(JsSafeInteger::MAX)
        .execute(&mut db)
        .await
        .unwrap();

    let command = || {
        AttachmentContentUpdateCommand::new(
            user("owner"),
            attachment("a"),
            AttachmentContentType::parse("image/png").unwrap(),
            8,
            blob("attachments/c/new.png"),
        )
        .unwrap()
    };
    let failure = writes.update_content(command()).await.unwrap_err();
    assert!(matches!(
        write_source(&failure),
        AttachmentWriteStoreError::RevisionExhausted
    ));
    let unchanged =
        sqlx::query("SELECT storage_key,content_revision FROM attachments WHERE id='a'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(
        unchanged.get::<String, _>("storage_key"),
        "attachments/c/old.png"
    );
    assert_eq!(
        unchanged.get::<i64, _>("content_revision"),
        JsSafeInteger::MAX
    );

    sqlx::query("UPDATE attachments SET content_revision=1 WHERE id='a'")
        .execute(&mut db)
        .await
        .unwrap();
    assert!(writes.update_content(command()).await.unwrap().is_ok());
}

#[tokio::test]
async fn delete_rechecks_owner_and_references_then_commits_cleanup_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attachment-delete.sqlite");
    seed(&path).await;
    let writes = MessagingStore::open(&path)
        .await
        .unwrap()
        .attachment_writes();
    writes
        .create(create_command("a", "owner", "attachments/c/a.png"))
        .await
        .unwrap()
        .unwrap();
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO messages(id,conversation_id,from_kind,content,metadata,seq,created_at)\
         VALUES('m','c','system','',?,1,1)",
    )
    .bind(serde_json::json!({"attachments":[{"id":"a"}]}).to_string())
    .execute(&mut db)
    .await
    .unwrap();

    assert_eq!(
        writes
            .delete(AttachmentDeleteCommand::new(user("other"), attachment("a")))
            .await
            .unwrap(),
        Err(AttachmentDeleteRefusal::Forbidden)
    );
    assert_eq!(
        writes
            .delete(AttachmentDeleteCommand::new(user("owner"), attachment("a")))
            .await
            .unwrap(),
        Err(AttachmentDeleteRefusal::InUse)
    );
    sqlx::query("DELETE FROM messages WHERE id='m'")
        .execute(&mut db)
        .await
        .unwrap();
    let deleted = writes
        .delete(AttachmentDeleteCommand::new(user("owner"), attachment("a")))
        .await
        .unwrap()
        .unwrap()
        .bind(&user("owner"), &attachment("a"))
        .unwrap();
    assert_eq!(
        deleted.deleted().blob().key().as_str(),
        "attachments/c/a.png"
    );
    assert!(!deleted.alias().encoded().is_empty());

    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE id='a'")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn delete_reads_references_only_after_its_writer_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attachment-delete-reservation.sqlite");
    seed(&path).await;
    let writes = MessagingStore::open(&path)
        .await
        .unwrap()
        .attachment_writes();
    writes
        .create(create_command("a", "owner", "attachments/c/a.png"))
        .await
        .unwrap()
        .unwrap();
    let mut blocker = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    blocker.execute("BEGIN IMMEDIATE").await.unwrap();
    let pending = {
        let writes = writes.clone();
        tokio::spawn(async move {
            writes
                .delete(AttachmentDeleteCommand::new(user("owner"), attachment("a")))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(!pending.is_finished());
    sqlx::query(
        "INSERT INTO messages(id,conversation_id,from_kind,content,metadata,seq,created_at)\
         VALUES('late-reference','c','system','',?,1,1)",
    )
    .bind(serde_json::json!({"attachments":[{"id":"a"}]}).to_string())
    .execute(&mut blocker)
    .await
    .unwrap();
    blocker.execute("COMMIT").await.unwrap();

    assert_eq!(
        pending.await.unwrap().unwrap(),
        Err(AttachmentDeleteRefusal::InUse)
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE id='a'")
        .fetch_one(&mut blocker)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn delete_fails_closed_on_unknown_metadata_then_repairs_through_the_same_port() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attachment-delete-corruption.sqlite");
    seed(&path).await;
    let writes = MessagingStore::open(&path)
        .await
        .unwrap()
        .attachment_writes();
    writes
        .create(create_command("a", "owner", "attachments/c/a.png"))
        .await
        .unwrap()
        .unwrap();
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO messages(id,conversation_id,from_kind,content,metadata,seq,created_at)\
         VALUES('corrupt','c','system','',?,1,1)",
    )
    .bind("{not-json")
    .execute(&mut db)
    .await
    .unwrap();
    let corrupt_row: i64 = sqlx::query_scalar("SELECT rowid FROM messages WHERE id='corrupt'")
        .fetch_one(&mut db)
        .await
        .unwrap();

    for corrupt_metadata in ["{not-json", r#"{"attachments":{}}"#] {
        sqlx::query("UPDATE messages SET metadata=? WHERE id='corrupt'")
            .bind(corrupt_metadata)
            .execute(&mut db)
            .await
            .unwrap();
        let failure = writes
            .delete(AttachmentDeleteCommand::new(user("owner"), attachment("a")))
            .await
            .unwrap_err();
        assert!(matches!(
            write_source(&failure),
            AttachmentWriteStoreError::MalformedMessageMetadata { row }
                if *row == corrupt_row
        ));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE id='a'")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(
            count, 1,
            "unknown reference state must leave the row intact"
        );
    }

    sqlx::query("UPDATE messages SET metadata=? WHERE id='corrupt'")
        .bind(serde_json::json!({"attachments":[{"id":"unrelated"}]}).to_string())
        .execute(&mut db)
        .await
        .unwrap();
    db.execute("PRAGMA foreign_keys=OFF").await.unwrap();
    sqlx::query(
        "INSERT INTO messages(id,conversation_id,from_kind,content,metadata,seq,created_at)\
         VALUES('','','system','',?,2,1)",
    )
    .bind(serde_json::json!({"attachments":[{"id":"also-unrelated"}]}).to_string())
    .execute(&mut db)
    .await
    .unwrap();
    assert!(
        writes
            .delete(AttachmentDeleteCommand::new(user("owner"), attachment("a")))
            .await
            .unwrap()
            .is_ok()
    );
}
