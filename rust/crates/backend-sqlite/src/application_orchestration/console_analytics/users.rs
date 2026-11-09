use super::{SqliteConsoleAnalyticsError, SqliteConsoleAnalyticsRead};
use silverhand_backend_core::application_orchestration::console_analytics::{
    CalendarWindow, ConsoleUserActivity, ConsoleUserDay, ConsoleUserDetail, ConsoleUsersQuery,
    ConsoleUsersSnapshot, TOP_USERS_LIMIT, USER_PAGE_SIZE,
};
use sqlx::{Row, sqlite::SqliteRow};

// Materialize the window once for both the global Top 10 and the searched page.
// Stable ID ordering resolves tied counts. Search does not change the global Top.
const USERS_SQL: &str = "WITH messages_in_window AS MATERIALIZED (
    SELECT from_user_id AS user_id, COUNT(*) AS message_count,
      COUNT(DISTINCT date(created_at / 1000, 'unixepoch', '+8 hours')) AS active_days,
      COUNT(DISTINCT conversation_id) AS conversation_count, MAX(created_at) AS last_message_at
    FROM messages INDEXED BY idx_messages_product_activity_created_at
    WHERE from_kind IN ('user','agent') AND from_kind='user'
      AND cc_source_message_id IS NULL AND created_at>=?1 AND created_at<=?2
    GROUP BY from_user_id
  ), auth AS (
    SELECT user_id, MAX(last_used_at) AS last_authenticated_at
    FROM auth_sessions INDEXED BY idx_auth_sessions_last_used_at
    WHERE last_used_at>=?1 AND last_used_at<=?2 GROUP BY user_id
  ), activity AS MATERIALIZED (
    SELECT u.id AS user_id, u.name, u.display_name,
      COALESCE(m.message_count,0) AS message_count, COALESCE(m.active_days,0) AS active_days,
      COALESCE(m.conversation_count,0) AS conversation_count,
      m.last_message_at, a.last_authenticated_at
    FROM users u LEFT JOIN messages_in_window m ON m.user_id=u.id
      LEFT JOIN auth a ON a.user_id=u.id
  ), ranked AS MATERIALIZED (
    SELECT *, ROW_NUMBER() OVER (ORDER BY message_count DESC,user_id) AS rank FROM activity
  ), page AS MATERIALIZED (
    SELECT user_id FROM ranked
    WHERE ?3='' OR instr(lower(name),lower(?3))>0 OR instr(lower(display_name),lower(?3))>0 OR user_id=?3
    ORDER BY rank LIMIT ?4 OFFSET ?5
  )
  SELECT r.*, p.user_id IS NOT NULL AS in_page,
    (r.rank<=?6 AND r.message_count>0) AS in_top
  FROM ranked r LEFT JOIN page p ON r.user_id=p.user_id
  WHERE p.user_id IS NOT NULL OR (r.rank<=?6 AND r.message_count>0)
  ORDER BY r.rank";

fn user_from_row(row: &SqliteRow) -> Result<ConsoleUserActivity, sqlx::Error> {
    Ok(ConsoleUserActivity {
        user_id: row.try_get("user_id")?,
        name: row.try_get("name")?,
        display_name: row.try_get("display_name")?,
        message_count: row.try_get("message_count")?,
        active_days: row.try_get("active_days")?,
        conversation_count: row.try_get("conversation_count")?,
        last_message_at: row.try_get("last_message_at")?,
        last_authenticated_at: row.try_get("last_authenticated_at")?,
    })
}

fn calendar(now_ms: i64, days: i64) -> Result<CalendarWindow, sqlx::Error> {
    if !matches!(days, 7 | 30) {
        return Err(sqlx::Error::Protocol("unsupported console window".into()));
    }
    CalendarWindow::ending_at(now_ms, days)
        .ok_or_else(|| sqlx::Error::Protocol("unsupported console calendar".into()))
}

impl SqliteConsoleAnalyticsRead {
    pub(super) async fn read_users_inner(
        &self,
        query: ConsoleUsersQuery,
        now_ms: i64,
    ) -> Result<ConsoleUsersSnapshot, SqliteConsoleAnalyticsError> {
        if !query.is_valid() {
            return Err(sqlx::Error::Protocol("invalid console users query".into()).into());
        }
        let calendar = calendar(now_ms, query.days)?;
        let start = calendar.start_ms().expect("nonempty calendar");
        let mut transaction = self.pool.begin().await?;
        let total_matching = sqlx::query_scalar(
            "SELECT COUNT(*) FROM users WHERE ?1='' OR instr(lower(name),lower(?1))>0 \
             OR instr(lower(display_name),lower(?1))>0 OR id=?1",
        )
        .bind(&query.search)
        .fetch_one(&mut *transaction)
        .await?;
        let rows = sqlx::query(USERS_SQL)
            .bind(start)
            .bind(now_ms)
            .bind(&query.search)
            .bind(USER_PAGE_SIZE)
            .bind((query.page - 1) * USER_PAGE_SIZE)
            .bind(TOP_USERS_LIMIT)
            .fetch_all(&mut *transaction)
            .await?;
        let mut users = Vec::new();
        let mut top_users = Vec::new();
        for row in rows {
            let user = user_from_row(&row)?;
            if row.try_get::<bool, _>("in_top")? {
                top_users.push(user.clone());
            }
            if row.try_get::<bool, _>("in_page")? {
                users.push(user);
            }
        }
        transaction.commit().await?;

        Ok(ConsoleUsersSnapshot {
            generated_at_ms: now_ms,
            total_matching,
            users,
            top_users,
        })
    }

    pub(super) async fn read_user_inner(
        &self,
        user_id: String,
        days: i64,
        now_ms: i64,
    ) -> Result<Option<ConsoleUserDetail>, SqliteConsoleAnalyticsError> {
        let calendar = calendar(now_ms, days)?;
        let start = calendar.start_ms().expect("nonempty calendar");
        let mut transaction = self.pool.begin().await?;
        let identity = sqlx::query("SELECT id,name,display_name FROM users WHERE id=?")
            .bind(&user_id)
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(identity) = identity else {
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT COUNT(*) AS message_count,
             COUNT(DISTINCT date(created_at / 1000,'unixepoch','+8 hours')) AS active_days,
             COUNT(DISTINCT conversation_id) AS conversation_count, MAX(created_at) AS last_message_at
             FROM messages INDEXED BY idx_messages_profile_user
             WHERE from_user_id=? AND from_kind='user' AND cc_source_message_id IS NULL
               AND created_at>=? AND created_at<=?",
        ).bind(&user_id).bind(start).bind(now_ms).fetch_one(&mut *transaction).await?;
        let last_authenticated_at = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT MAX(last_used_at) FROM auth_sessions WHERE user_id=? AND last_used_at>=? AND last_used_at<=?",
        ).bind(&user_id).bind(start).bind(now_ms).fetch_one(&mut *transaction).await?;
        let user = ConsoleUserActivity {
            user_id: identity.try_get("id")?,
            name: identity.try_get("name")?,
            display_name: identity.try_get("display_name")?,
            message_count: row.try_get("message_count")?,
            active_days: row.try_get("active_days")?,
            conversation_count: row.try_get("conversation_count")?,
            last_message_at: row.try_get("last_message_at")?,
            last_authenticated_at,
        };
        let bounds = serde_json::to_string(&calendar.bounds)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let rows = sqlx::query(
            "SELECT CAST(d.key AS INTEGER) AS day_index, COUNT(m.id) AS message_count
             FROM json_each(?) d LEFT JOIN messages m INDEXED BY idx_messages_profile_user
               ON m.from_user_id=? AND m.from_kind='user' AND m.cc_source_message_id IS NULL
               AND m.created_at>=json_extract(d.value,'$[0]') AND m.created_at<json_extract(d.value,'$[1]')
               AND m.created_at<=?
             GROUP BY d.key ORDER BY CAST(d.key AS INTEGER)",
        ).bind(bounds).bind(&user_id).bind(now_ms).fetch_all(&mut *transaction).await?;
        let daily_activity = rows
            .into_iter()
            .map(|row| {
                let index: i64 = row.try_get("day_index")?;
                Ok(ConsoleUserDay {
                    date: calendar.dates[index as usize].clone(),
                    message_count: row.try_get("message_count")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        transaction.commit().await?;

        Ok(Some(ConsoleUserDetail {
            generated_at_ms: now_ms,
            user,
            daily_activity,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::application_orchestration::console_analytics::ConsoleAnalyticsRead;
    use sqlx::{Connection, Executor};

    #[tokio::test]
    async fn users_are_ranked_paged_searchable_and_details_use_the_same_calendar() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("users.sqlite");
        let mut database = crate::current_schema::create_current_schema(&path).await;
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-09T13:00:00+08:00")
            .unwrap()
            .timestamp_millis();
        let start = chrono::DateTime::parse_from_rfc3339("2026-09-03T00:00:00+08:00")
            .unwrap()
            .timestamp_millis();
        for index in 0..28 {
            sqlx::query(
                "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES (?,?,?,0,0)",
            )
            .bind(format!("u{index:02}"))
            .bind(format!("user{index:02}"))
            .bind(format!("Person {index:02}"))
            .execute(&mut database)
            .await
            .unwrap();
        }
        database.execute(sqlx::raw_sql(
            "INSERT INTO workspaces(id,name,name_key,created_at,updated_at,owner_user_id) VALUES ('w','Work','work',0,0,'u00');
             INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,created_at,updated_at)
             VALUES ('c','w','workspace','open','General','general',0,0);"
        )).await.unwrap();
        for (id, user, created_at, copy, kind) in [
            ("one", "u00", start, None, "user"),
            ("two", "u00", now, None, "user"),
            ("three", "u01", now, None, "user"),
            ("four", "u02", now, None, "user"),
            ("old", "u00", start - 1, None, "user"),
            ("future", "u00", now + 1, None, "user"),
            ("copy", "u00", now, Some("one"), "user"),
            ("system", "u00", now, None, "system"),
        ] {
            sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,cc_source_message_id) VALUES (?,'c',?,?,'private body',(SELECT COALESCE(MAX(seq),0)+1 FROM messages),?,?)")
                .bind(id).bind(kind).bind(if kind=="user" {Some(user)} else {None}).bind(created_at).bind(copy)
                .execute(&mut database).await.unwrap();
        }
        for index in 3..15 {
            sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES (?,'c','user',?,'private body',(SELECT MAX(seq)+1 FROM messages),?)")
                .bind(format!("rank-{index}")).bind(format!("u{index:02}")).bind(now)
                .execute(&mut database).await.unwrap();
        }
        sqlx::query("INSERT INTO auth_sessions(id,user_id,token_hash,created_at,last_used_at) VALUES ('s','u00','hash',0,?),('future','u00','future-hash',0,?)")
            .bind(now-1000).bind(now+1).execute(&mut database).await.unwrap();
        database.close().await.unwrap();
        let read = SqliteConsoleAnalyticsRead::open(&path).await.unwrap();
        let query = ConsoleUsersQuery {
            days: 7,
            search: String::new(),
            page: 1,
        };
        let first = read.read_users(query.clone(), now).await.unwrap();

        assert_eq!(first.total_matching, 28);
        assert_eq!(first.users.len(), 25);
        assert_eq!(
            first
                .top_users
                .iter()
                .map(|user| user.user_id.as_str())
                .collect::<Vec<_>>(),
            (0..10)
                .map(|index| format!("u{index:02}"))
                .collect::<Vec<_>>()
        );
        let winner = &first.top_users[0];
        assert_eq!(winner.message_count, 2);
        assert_eq!(winner.active_days, 2);
        assert_eq!(winner.conversation_count, 1);
        assert_eq!(winner.last_authenticated_at, Some(now - 1000));
        let second = read
            .read_users(
                ConsoleUsersQuery {
                    page: 2,
                    ..query.clone()
                },
                now,
            )
            .await
            .unwrap();
        assert_eq!(second.users.len(), 3);
        assert_eq!(second.users[0].user_id, "u25");
        assert_eq!(second.users[0].message_count, 0);
        let search = read
            .read_users(
                ConsoleUsersQuery {
                    search: "PERSON 27".into(),
                    ..query.clone()
                },
                now,
            )
            .await
            .unwrap();
        assert_eq!(search.total_matching, 1);
        assert_eq!(search.users[0].user_id, "u27");
        assert_eq!(search.top_users, first.top_users);
        let injection = read
            .read_users(
                ConsoleUsersQuery {
                    search: "%' OR 1=1 --".into(),
                    ..query.clone()
                },
                now,
            )
            .await
            .unwrap();
        assert_eq!(injection.total_matching, 0);
        assert!(injection.users.is_empty());
        assert_eq!(injection.top_users, first.top_users);

        let detail = read.read_user("u00".into(), 7, now).await.unwrap().unwrap();
        assert_eq!(&detail.user, winner);
        assert_eq!(detail.daily_activity.len(), 7);
        assert_eq!(detail.daily_activity[0].message_count, 1);
        assert_eq!(detail.daily_activity[6].message_count, 1);
        assert_eq!(
            detail
                .daily_activity
                .iter()
                .map(|day| day.message_count)
                .sum::<i64>(),
            2
        );
        let longer = read
            .read_user("u00".into(), 30, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(longer.user.message_count, 3);
        assert_eq!(longer.daily_activity.len(), 30);
        assert!(
            read.read_user("missing".into(), 7, now)
                .await
                .unwrap()
                .is_none()
        );
        let silent = read.read_user("u27".into(), 7, now).await.unwrap().unwrap();
        assert_eq!(silent.user.message_count, 0);
        assert!(silent.user.last_message_at.is_none());
        assert!(
            silent
                .daily_activity
                .iter()
                .all(|day| day.message_count == 0)
        );
        assert!(
            read.read_users(ConsoleUsersQuery { days: 365, ..query }, now)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn user_ranking_uses_window_indexes_and_returns_empty_on_an_empty_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plans.sqlite");
        let mut database = crate::current_schema::create_current_schema(&path).await;
        // The statement is a fixed application constant; no user input enters SQL text.
        let plan = sqlx::query(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {USERS_SQL}"
        )))
        .bind(0_i64)
        .bind(1_i64)
        .bind("")
        .bind(USER_PAGE_SIZE)
        .bind(0_i64)
        .bind(TOP_USERS_LIMIT)
        .fetch_all(&mut database)
        .await
        .unwrap();
        let details = plan
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            details.contains("idx_messages_product_activity_created_at"),
            "{details}"
        );
        assert!(
            details.contains("idx_auth_sessions_last_used_at"),
            "{details}"
        );
        database.close().await.unwrap();
        let read = SqliteConsoleAnalyticsRead::open(&path).await.unwrap();
        let empty = read
            .read_users(
                ConsoleUsersQuery {
                    days: 7,
                    search: String::new(),
                    page: 1,
                },
                1,
            )
            .await
            .unwrap();
        assert_eq!(empty.total_matching, 0);
        assert!(empty.top_users.is_empty());
        assert!(empty.users.is_empty());
    }
}
