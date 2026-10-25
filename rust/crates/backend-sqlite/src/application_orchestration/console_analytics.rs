mod users;

use silverhand_backend_core::{
    application_orchestration::console_analytics::{
        ActivityWindow, CalendarWindow, CategoryCount, ConsoleAnalyticsRead,
        ConsoleAnalyticsSnapshot, ConsoleTotals, ConsoleUserDetail, ConsoleUsersQuery,
        ConsoleUsersSnapshot, DailyActivity, TREND_DAYS,
    },
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, SqlitePool};
use std::{path::Path, time::Duration};
use thiserror::Error;

const WINDOW_SESSIONS_SQL: &str = "SELECT COUNT(*) FROM (SELECT user_id FROM auth_sessions \
             INDEXED BY idx_auth_sessions_last_used_at \
             WHERE last_used_at>=? AND last_used_at<=? GROUP BY user_id)";
const WINDOW_ACTIVITY_SQL: &str = "SELECT COUNT(DISTINCT CASE WHEN from_kind='user' THEN from_user_id END) AS speakers, \
             SUM(CASE WHEN from_kind='user' THEN 1 ELSE 0 END) AS human_messages, \
             SUM(CASE WHEN from_kind='agent' THEN 1 ELSE 0 END) AS agent_messages \
             FROM messages WHERE created_at>=? AND created_at<=? \
             AND from_kind IN ('user','agent') \
             AND cc_source_message_id IS NULL";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum SqliteConsoleAnalyticsError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("database path is not valid UTF-8")]
    InvalidPath,
}

#[derive(Clone)]
pub struct SqliteConsoleAnalyticsRead {
    pool: SqlitePool,
}

impl SqliteConsoleAnalyticsRead {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteConsoleAnalyticsError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;

        Ok(Self { pool })
    }

    async fn read_window(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        days: i64,
        start_ms: i64,
        now_ms: i64,
    ) -> Result<ActivityWindow, sqlx::Error> {
        let authenticated_users = sqlx::query_scalar::<_, i64>(WINDOW_SESSIONS_SQL)
            .bind(start_ms)
            .bind(now_ms)
            .fetch_one(&mut **transaction)
            .await?;
        let row = sqlx::query(WINDOW_ACTIVITY_SQL)
            .bind(start_ms)
            .bind(now_ms)
            .fetch_one(&mut **transaction)
            .await?;

        Ok(ActivityWindow {
            days,
            authenticated_users,
            speaking_users: row.try_get("speakers")?,
            human_messages: row
                .try_get::<Option<i64>, _>("human_messages")?
                .unwrap_or(0),
            agent_messages: row
                .try_get::<Option<i64>, _>("agent_messages")?
                .unwrap_or(0),
        })
    }
}

impl ConsoleAnalyticsRead for SqliteConsoleAnalyticsRead {
    async fn read_users(
        &self,
        query: ConsoleUsersQuery,
        now_ms: i64,
    ) -> Result<ConsoleUsersSnapshot, PersistenceFailure> {
        self.read_users_inner(query, now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_user(
        &self,
        user_id: String,
        days: i64,
        now_ms: i64,
    ) -> Result<Option<ConsoleUserDetail>, PersistenceFailure> {
        self.read_user_inner(user_id, days, now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_snapshot(
        &self,
        now_ms: i64,
    ) -> Result<ConsoleAnalyticsSnapshot, PersistenceFailure> {
        self.read_snapshot_inner(now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

impl SqliteConsoleAnalyticsRead {
    async fn read_snapshot_inner(
        &self,
        now_ms: i64,
    ) -> Result<ConsoleAnalyticsSnapshot, SqliteConsoleAnalyticsError> {
        let calendar = CalendarWindow::ending_at(now_ms, TREND_DAYS).ok_or_else(|| {
            SqliteConsoleAnalyticsError::Sql(sqlx::Error::Protocol(
                "system clock is outside the supported calendar".to_owned(),
            ))
        })?;
        let seven_start = calendar.bounds[(TREND_DAYS - 7) as usize].0;
        let thirty_start = calendar.start_ms().expect("a 30-day window is nonempty");

        let mut transaction = self.pool.begin().await?;
        let totals = sqlx::query(
            "SELECT \
             (SELECT COUNT(*) FROM users) AS users, \
             (SELECT COUNT(*) FROM agents) AS agents, \
             (SELECT COUNT(*) FROM workspaces WHERE owner_agent_id IS NULL) AS regular_workspaces, \
             (SELECT COUNT(*) FROM conversations WHERE placement_kind='workspace' AND archived_at IS NULL) AS active_channels, \
             (SELECT COUNT(*) FROM conversations WHERE placement_kind='workspace' AND archived_at IS NOT NULL) AS archived_channels, \
             (SELECT COUNT(*) FROM messages WHERE from_kind IN ('user','agent') AND cc_source_message_id IS NULL) AS messages, \
             (SELECT COUNT(*) FROM hosts WHERE revoked_at IS NULL) AS active_computers",
        )
        .fetch_one(&mut *transaction)
        .await?;
        let totals = ConsoleTotals {
            users: totals.try_get("users")?,
            agents: totals.try_get("agents")?,
            regular_workspaces: totals.try_get("regular_workspaces")?,
            active_channels: totals.try_get("active_channels")?,
            archived_channels: totals.try_get("archived_channels")?,
            messages: totals.try_get("messages")?,
            active_computers: totals.try_get("active_computers")?,
        };

        let windows = vec![
            Self::read_window(&mut transaction, 7, seven_start, now_ms).await?,
            Self::read_window(&mut transaction, TREND_DAYS, thirty_start, now_ms).await?,
        ];

        let rows = sqlx::query(
            "SELECT CAST(d.key AS INTEGER) AS day_index, \
             COUNT(DISTINCT CASE WHEN m.from_kind='user' THEN m.from_user_id END) AS speakers, \
             SUM(CASE WHEN m.from_kind='user' THEN 1 ELSE 0 END) AS human_messages, \
             SUM(CASE WHEN m.from_kind='agent' THEN 1 ELSE 0 END) AS agent_messages \
             FROM json_each(?) AS d LEFT JOIN messages AS m \
             ON m.created_at>=json_extract(d.value,'$[0]') \
             AND m.created_at<json_extract(d.value,'$[1]') \
             AND m.created_at<=? \
             AND m.from_kind IN ('user','agent') AND m.cc_source_message_id IS NULL \
             GROUP BY d.key ORDER BY CAST(d.key AS INTEGER)",
        )
        .bind(serde_json::to_string(&calendar.bounds).map_err(|error| {
            SqliteConsoleAnalyticsError::Sql(sqlx::Error::Encode(Box::new(error)))
        })?)
        .bind(now_ms)
        .fetch_all(&mut *transaction)
        .await?;
        let daily_activity =
            rows.into_iter()
                .map(|row| {
                    let index: i64 = row.try_get("day_index")?;
                    let date = calendar.dates.get(index as usize).cloned().ok_or_else(|| {
                        sqlx::Error::Protocol("daily index outside calendar".into())
                    })?;
                    Ok(DailyActivity {
                        date,
                        speaking_users: row.try_get("speakers")?,
                        human_messages: row
                            .try_get::<Option<i64>, _>("human_messages")?
                            .unwrap_or(0),
                        agent_messages: row
                            .try_get::<Option<i64>, _>("agent_messages")?
                            .unwrap_or(0),
                    })
                })
                .collect::<Result<Vec<_>, sqlx::Error>>()?;

        let agent_runtimes = sqlx::query(
            "SELECT runtime,COUNT(*) AS count FROM agents GROUP BY runtime ORDER BY runtime",
        )
        .fetch_all(&mut *transaction)
        .await?
        .into_iter()
        .map(|row| {
            Ok(CategoryCount {
                category: row.try_get("runtime")?,
                count: row.try_get("count")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
        transaction.commit().await?;

        Ok(ConsoleAnalyticsSnapshot {
            generated_at_ms: now_ms,
            totals,
            windows,
            daily_activity,
            agent_runtimes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use silverhand_backend_core::application_orchestration::console_analytics::ConsoleAnalyticsRead;
    use sqlx::{Connection, Executor};

    #[tokio::test]
    async fn snapshot_is_bounded_source_free_and_excludes_delivery_copies() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("console.sqlite");
        let mut database = crate::current_schema::create_current_schema(&path).await;
        let now = DateTime::parse_from_rfc3339("2026-09-09T13:00:00+08:00")
            .unwrap()
            .timestamp_millis();
        let day = 86_400_000_i64;

        database
            .execute(sqlx::raw_sql(
                "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
                   ('u1','one','One',0,0),('u2','two','Two',0,0); \
                 INSERT INTO hosts(id,name,kind,created_at,user_id,revoked_at) VALUES \
                   ('h1','host-one','desktop',0,'u1',NULL),('h2','host-two','desktop',0,'u2',1); \
                 INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) \
                   VALUES ('a1','agent-one','pi','h1',0,0,'u1'); \
                 INSERT INTO workspaces(id,name,name_key,created_at,updated_at,owner_user_id) \
                   VALUES ('w1','Work','work',0,0,'u1'); \
                 INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at) \
                   VALUES ('c1','w1','workspace','open','General','general','active',0,0);",
            ))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO auth_sessions(id,user_id,token_hash,created_at,last_used_at) \
             VALUES ('s1','u1','hash-one',0,?),('s2','u2','hash-two',0,?), \
             ('s3','u2','hash-future',0,?)",
        )
        .bind(now - day)
        .bind(now - 10 * day)
        .bind(now + day)
        .execute(&mut database)
        .await
        .unwrap();
        for (id, kind, user, content, seq, created_at, copy) in [
            ("m1", "user", Some("u1"), "recent", 1, now - day, None),
            ("m2", "agent", None, "agent", 2, now - day, None),
            ("m3", "user", Some("u2"), "older", 3, now - 10 * day, None),
            ("m4", "user", Some("u1"), "copy", 4, now - day, Some("m1")),
            ("m5", "system", None, "event", 5, now - day, None),
            ("m6", "user", Some("u2"), "future", 6, now + day, None),
        ] {
            sqlx::query(
                "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,cc_source_message_id) \
                 VALUES (?,'c1',?,?,?,?,?,?)",
            )
            .bind(id)
            .bind(kind)
            .bind(user)
            .bind(content)
            .bind(seq)
            .bind(created_at)
            .bind(copy)
            .execute(&mut database)
            .await
            .unwrap();
        }
        database.close().await.unwrap();

        let read = SqliteConsoleAnalyticsRead::open(&path).await.unwrap();
        let snapshot = read.read_snapshot(now).await.unwrap();

        assert_eq!(snapshot.totals.users, 2);
        assert_eq!(snapshot.totals.agents, 1);
        assert_eq!(snapshot.totals.regular_workspaces, 1);
        assert_eq!(snapshot.totals.active_channels, 1);
        assert_eq!(snapshot.totals.messages, 4);
        assert_eq!(snapshot.totals.active_computers, 1);
        assert_eq!(snapshot.windows[0].authenticated_users, 1);
        assert_eq!(snapshot.windows[0].speaking_users, 1);
        assert_eq!(snapshot.windows[0].human_messages, 1);
        assert_eq!(snapshot.windows[0].agent_messages, 1);
        assert_eq!(snapshot.windows[1].authenticated_users, 2);
        assert_eq!(snapshot.windows[1].speaking_users, 2);
        assert_eq!(snapshot.windows[1].human_messages, 2);
        assert_eq!(snapshot.daily_activity.len(), 30);
        assert_eq!(snapshot.agent_runtimes[0].category, "pi");
    }

    #[tokio::test]
    async fn time_bounded_queries_use_the_console_indexes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plans.sqlite");
        let mut database = crate::current_schema::create_current_schema(&path).await;

        let session_plan = sqlx::query(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {WINDOW_SESSIONS_SQL}"
        )))
        .bind(0_i64)
        .bind(1_i64)
        .fetch_all(&mut database)
        .await
        .unwrap();
        let message_plan = sqlx::query(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {WINDOW_ACTIVITY_SQL}"
        )))
        .bind(0_i64)
        .bind(1_i64)
        .fetch_all(&mut database)
        .await
        .unwrap();
        let session_details = session_plan
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>()
            .join("\n");
        let message_details = message_plan
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            session_details.contains("idx_auth_sessions_last_used_at"),
            "unexpected auth-session query plan:\n{session_details}"
        );
        assert!(
            message_details.contains("idx_messages_product_activity_created_at"),
            "unexpected message query plan:\n{message_details}"
        );
    }
}
