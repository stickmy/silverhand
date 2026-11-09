//! SQLite adapter for self-service account deletion.

use silverhand_backend_core::{
    foundation::identity::{AgentId, HostId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::{
        account_deletion::{
            AccountDeletionBlockers, AccountDeletionStore, AccountDeletionWrite, DeletedAccount,
            OwnedAgent, OwnedWorkspace,
        },
        auth::SessionTokenHash,
        avatar::AvatarBlobPointer,
        user_identity::ParticipantHandle,
    },
};
use sqlx::{Row, SqliteConnection};
use thiserror::Error;

use super::user_identity::SqliteUserStore;

// Agents on the user's computers block too: `agents.host_id` is RESTRICT and the
// user's hosts cascade with the user row.
const SELECT_BLOCKING_AGENTS: &str = "\
SELECT id, name FROM agents \
 WHERE created_by_user_id = ?1 \
    OR host_id IN (SELECT id FROM hosts WHERE user_id = ?1) \
 ORDER BY created_at, id";
const SELECT_OWNED_WORKSPACES: &str = "\
SELECT w.id, w.name FROM workspaces w \
 WHERE w.owner_agent_id IS NULL \
   AND (w.owner_user_id = ?1 OR EXISTS ( \
        SELECT 1 FROM workspace_users wu \
         WHERE wu.workspace_id = w.id AND wu.user_id = ?1 AND wu.role = 'owner')) \
 ORDER BY w.created_at, w.id";
const SELECT_IDENTITY_UNDER_LOCK: &str = "\
SELECT u.avatar_storage_key FROM users u \
  JOIN user_password_credentials c ON c.user_id = u.id \
  JOIN auth_sessions s ON s.user_id = u.id \
 WHERE u.id = ? AND u.name = ? AND c.password_hash = ? \
   AND s.token_hash = ? AND s.revoked_at IS NULL";
const SELECT_HOSTS: &str = "SELECT id FROM hosts WHERE user_id = ? ORDER BY id";
// Rows with no FK to `users` that only mean something while the person exists.
const DELETE_UNLINKED: [&str; 3] = [
    "DELETE FROM notifications WHERE user_id = ?",
    "DELETE FROM message_reactions WHERE reactor_kind = 'user' AND reactor_id = ?",
    "DELETE FROM thread_followers WHERE principal_kind = 'human' AND principal_id = ?",
];
const DELETE_USER: &str = "DELETE FROM users WHERE id = ?";

#[derive(Debug, Error)]
enum AccountDeletionStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored identity {0} is malformed")]
    MalformedIdentity(&'static str),
}

fn failure(error: impl Into<AccountDeletionStoreError>) -> PersistenceFailure {
    PersistenceFailure::from_source(error.into())
}

async fn read_blockers(
    conn: &mut SqliteConnection,
    user: &UserId,
) -> Result<AccountDeletionBlockers, PersistenceFailure> {
    let mut blockers = AccountDeletionBlockers::default();
    for row in sqlx::query(SELECT_BLOCKING_AGENTS)
        .bind(user.as_str())
        .fetch_all(&mut *conn)
        .await
        .map_err(failure)?
    {
        blockers.agents.push(OwnedAgent {
            id: AgentId::parse(row.try_get::<String, _>("id").map_err(failure)?)
                .map_err(|_| failure(AccountDeletionStoreError::MalformedIdentity("agents.id")))?,
            name: row.try_get("name").map_err(failure)?,
        });
    }
    for row in sqlx::query(SELECT_OWNED_WORKSPACES)
        .bind(user.as_str())
        .fetch_all(&mut *conn)
        .await
        .map_err(failure)?
    {
        blockers.workspaces.push(OwnedWorkspace {
            id: WorkspaceId::parse(row.try_get::<String, _>("id").map_err(failure)?).map_err(
                |_| {
                    failure(AccountDeletionStoreError::MalformedIdentity(
                        "workspaces.id",
                    ))
                },
            )?,
            name: row.try_get("name").map_err(failure)?,
        });
    }
    Ok(blockers)
}

impl AccountDeletionStore for SqliteUserStore {
    async fn deletion_blockers(
        &self,
        user: &UserId,
    ) -> Result<AccountDeletionBlockers, PersistenceFailure> {
        let mut conn = self.pool().acquire().await.map_err(failure)?;
        read_blockers(&mut conn, user).await
    }

    async fn password_for_user(&self, user: &UserId) -> Result<Option<String>, PersistenceFailure> {
        sqlx::query_scalar("SELECT password_hash FROM user_password_credentials WHERE user_id = ?")
            .bind(user.as_str())
            .fetch_optional(self.pool())
            .await
            .map_err(failure)
    }

    async fn delete_account(
        &self,
        user: &UserId,
        session: &SessionTokenHash,
        handle: &ParticipantHandle,
        expected_password_hash: &str,
    ) -> Result<AccountDeletionWrite, PersistenceFailure> {
        let mut tx = self
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failure)?;
        let Some(row) = sqlx::query(SELECT_IDENTITY_UNDER_LOCK)
            .bind(user.as_str())
            .bind(handle.as_str())
            .bind(expected_password_hash)
            .bind(session.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(failure)?
        else {
            return Ok(AccountDeletionWrite::Rejected);
        };
        let blockers = read_blockers(&mut tx, user).await?;
        if !blockers.is_clear() {
            return Ok(AccountDeletionWrite::Blocked(blockers));
        }
        let avatar = match row
            .try_get::<Option<String>, _>("avatar_storage_key")
            .map_err(failure)?
            .filter(|key| !key.is_empty())
        {
            Some(key) => Some(AvatarBlobPointer::parse(key).map_err(|_| {
                failure(AccountDeletionStoreError::MalformedIdentity(
                    "users.avatar_storage_key",
                ))
            })?),
            None => None,
        };
        let mut hosts = Vec::new();
        for raw in sqlx::query_scalar::<_, String>(SELECT_HOSTS)
            .bind(user.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(failure)?
        {
            hosts.push(
                HostId::parse(raw).map_err(|_| {
                    failure(AccountDeletionStoreError::MalformedIdentity("hosts.id"))
                })?,
            );
        }
        for statement in DELETE_UNLINKED {
            sqlx::query(statement)
                .bind(user.as_str())
                .execute(&mut *tx)
                .await
                .map_err(failure)?;
        }
        sqlx::query(DELETE_USER)
            .bind(user.as_str())
            .execute(&mut *tx)
            .await
            .map_err(failure)?;
        tx.commit().await.map_err(failure)?;
        Ok(AccountDeletionWrite::Deleted(DeletedAccount {
            user_id: user.clone(),
            avatar,
            hosts,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{
        Connection, Executor,
        sqlite::{SqliteConnectOptions, SqliteConnection},
    };

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    // user-1 is deleted; user-2 is the bystander whose content must survive.
    const SEED_SQL: &str = "\
INSERT INTO users (id,name,display_name,email,avatar_storage_key,avatar_content_type,created_at,updated_at) \
  VALUES ('user-1','alice','Alice','a@example.com','avatars/a.png','image/png',1,1), \
         ('user-2','bob','Bob','b@example.com',NULL,NULL,1,1); \
INSERT INTO user_password_credentials (user_id,password_hash,password_hash_params,password_updated_at) \
  VALUES ('user-1','phc-1','{}',1),('user-2','phc-2','{}',1); \
INSERT INTO auth_sessions (id,user_id,token_hash,created_at,last_used_at) \
  VALUES ('s1','user-1','tok-1',1,1),('s1b','user-1','tok-1b',1,1),('s2','user-2','tok-2',1,1); \
INSERT INTO hosts (id,name,kind,created_at,user_id,provisioning_source) VALUES ('h1','h1','remote',1,'user-1','user_invite'); \
INSERT INTO workspaces (id,name,name_key,owner_user_id,created_at,updated_at) VALUES ('ws','WS','ws','user-2',1,1); \
INSERT INTO workspace_users (workspace_id,user_id,joined_at,role) VALUES ('ws','user-2',1,'owner'),('ws','user-1',1,'member'); \
INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,visibility,created_at,updated_at) \
  VALUES ('c','ws','workspace','open','C','c','public',1,1); \
INSERT INTO conversation_users (conversation_id,user_id,joined_at) VALUES ('c','user-1',1),('c','user-2',1); \
INSERT INTO messages (id,conversation_id,from_kind,from_user_id,content,seq,created_at,author_id,author_name,author_display_name) \
  VALUES ('m1','c','user','user-1','hello from alice',1,1,'user-1','alice','Alice'), \
         ('m2','c','user','user-2','hello from bob',2,2,'user-2','bob','Bob'); \
INSERT INTO message_reactions (id,message_id,reactor_kind,reactor_id,emoji,created_at) \
  VALUES ('r1','m2','user','user-1','+1',1),('r2','m1','user','user-2','+1',1); \
INSERT INTO attachments (id,channel_id,filename,size,content_type,storage,storage_key,created_by_kind,created_by_id,created_at) \
  VALUES ('att','c','f.txt',1,'text/plain','local','attachments/f.bin','user','user-1',1);";

    async fn open_seeded() -> (tempfile::TempDir, SqliteUserStore, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("account.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let store = SqliteUserStore::open(&path).await.unwrap();
        (directory, store, path)
    }

    fn uid(raw: &str) -> UserId {
        UserId::parse(raw).unwrap()
    }

    fn handle(raw: &str) -> ParticipantHandle {
        ParticipantHandle::parse(raw).unwrap()
    }

    async fn count(store: &SqliteUserStore, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(store.pool())
            .await
            .unwrap()
    }

    async fn seat_token(store: &SqliteUserStore, id: &str, plaintext: &str) -> SessionTokenHash {
        let hash = SessionTokenHash::of_plaintext(plaintext);
        sqlx::query("UPDATE auth_sessions SET token_hash = ? WHERE id = ?")
            .bind(hash.as_str())
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();
        hash
    }

    #[tokio::test]
    async fn deletes_identity_and_keeps_shared_content() {
        let (_dir, store, _) = open_seeded().await;
        let session = seat_token(&store, "s1", "plain-1").await;
        let outcome = store
            .delete_account(&uid("user-1"), &session, &handle("alice"), "phc-1")
            .await
            .unwrap();
        let AccountDeletionWrite::Deleted(deleted) = outcome else {
            panic!("expected Deleted, got {outcome:?}");
        };
        assert_eq!(deleted.avatar.unwrap().as_str(), "avatars/a.png");
        assert_eq!(
            deleted.hosts.iter().map(HostId::as_str).collect::<Vec<_>>(),
            ["h1"]
        );

        for (sql, expected) in [
            ("SELECT COUNT(*) FROM users WHERE id='user-1'", 0),
            (
                "SELECT COUNT(*) FROM auth_sessions WHERE user_id='user-1'",
                0,
            ),
            (
                "SELECT COUNT(*) FROM user_password_credentials WHERE user_id='user-1'",
                0,
            ),
            (
                "SELECT COUNT(*) FROM workspace_users WHERE user_id='user-1'",
                0,
            ),
            (
                "SELECT COUNT(*) FROM conversation_users WHERE user_id='user-1'",
                0,
            ),
            ("SELECT COUNT(*) FROM hosts WHERE user_id='user-1'", 0),
            (
                "SELECT COUNT(*) FROM message_reactions WHERE reactor_id='user-1'",
                0,
            ),
            // Shared content and the bystander are untouched.
            ("SELECT COUNT(*) FROM messages", 2),
            (
                "SELECT COUNT(*) FROM messages WHERE id='m1' AND from_user_id IS NULL \
                 AND author_name='alice' AND content='hello from alice'",
                1,
            ),
            ("SELECT COUNT(*) FROM attachments WHERE id='att'", 1),
            (
                "SELECT COUNT(*) FROM message_reactions WHERE reactor_id='user-2'",
                1,
            ),
            ("SELECT COUNT(*) FROM users WHERE id='user-2'", 1),
            (
                "SELECT COUNT(*) FROM auth_sessions WHERE user_id='user-2'",
                1,
            ),
            (
                "SELECT COUNT(*) FROM workspace_users WHERE user_id='user-2'",
                1,
            ),
            (
                "SELECT COUNT(*) FROM conversation_users WHERE user_id='user-2'",
                1,
            ),
            ("SELECT COUNT(*) FROM workspaces WHERE id='ws'", 1),
        ] {
            assert_eq!(count(&store, sql).await, expected, "{sql}");
        }
    }

    #[tokio::test]
    async fn wrong_handle_password_or_revoked_session_deletes_nothing() {
        let (_dir, store, _) = open_seeded().await;
        let session = seat_token(&store, "s1", "plain-1").await;
        let other = seat_token(&store, "s2", "plain-2").await;
        for (session, handle_raw, phc) in [
            (&session, "bob", "phc-1"),
            (&session, "alice", "phc-2"),
            (&other, "alice", "phc-1"),
        ] {
            let outcome = store
                .delete_account(&uid("user-1"), session, &handle(handle_raw), phc)
                .await
                .unwrap();
            assert_eq!(outcome, AccountDeletionWrite::Rejected);
        }
        sqlx::query("UPDATE auth_sessions SET revoked_at = 1 WHERE id = 's1'")
            .execute(store.pool())
            .await
            .unwrap();
        let outcome = store
            .delete_account(&uid("user-1"), &session, &handle("alice"), "phc-1")
            .await
            .unwrap();
        assert_eq!(outcome, AccountDeletionWrite::Rejected);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM users WHERE id='user-1'").await,
            1
        );
    }

    #[tokio::test]
    async fn owned_agents_and_workspaces_block_deletion() {
        let (_dir, store, _) = open_seeded().await;
        let session = seat_token(&store, "s1", "plain-1").await;
        sqlx::raw_sql(
            "INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) \
               VALUES ('ag','helper','Helper','claude','h1','user-1',1,1,'agent'); \
             INSERT INTO workspaces (id,name,name_key,owner_user_id,created_at,updated_at) VALUES ('mine','Mine','mine','user-1',2,2); \
             INSERT INTO workspace_users (workspace_id,user_id,joined_at,role) VALUES ('mine','user-1',1,'owner');",
        )
        .execute(store.pool())
        .await
        .unwrap();
        let blockers = store.deletion_blockers(&uid("user-1")).await.unwrap();
        assert_eq!(blockers.agents.len(), 1);
        assert_eq!(blockers.agents[0].name, "helper");
        assert_eq!(blockers.workspaces.len(), 1);
        assert_eq!(blockers.workspaces[0].id.as_str(), "mine");
        let outcome = store
            .delete_account(&uid("user-1"), &session, &handle("alice"), "phc-1")
            .await
            .unwrap();
        assert_eq!(outcome, AccountDeletionWrite::Blocked(blockers));
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM users WHERE id='user-1'").await,
            1
        );
        // A member (not owner) of someone else's workspace is not blocked by it.
        assert!(
            store
                .deletion_blockers(&uid("user-2"))
                .await
                .unwrap()
                .workspaces
                .iter()
                .all(|w| w.id.as_str() == "ws")
        );
    }
}
