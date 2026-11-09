//! Inspect the adapter's executed SQL, not a second hand-written query.
//! This binary owns one global subscriber so SQLite worker events are captured.

#[macro_use]
#[path = "../../backend-sqlite/tests/support/seed_rows.rs"]
mod seed_rows;

use silverhand_backend_core::{
    foundation::identity::{AgentId, Principal, UserId},
    identity_access::profile_message_counts::{MessageCountYear, ProfileMessageCountRead},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor};
use std::sync::{Arc, Mutex};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};
#[path = "../../backend-sqlite/tests/support/current_schema_snapshot.rs"]
mod current_schema;

#[derive(Clone, Default)]
struct Statements(Arc<Mutex<Vec<String>>>);
#[derive(Default)]
struct SqlField(Option<String>);
impl Visit for SqlField {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "db.statement" {
            self.0 = Some(value.trim().to_owned());
        }
    }
    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}
impl<S: Subscriber> Layer<S> for Statements {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let mut field = SqlField::default();
        event.record(&mut field);
        if let Some(sql) = field.0.filter(|s| s.starts_with("SELECT")) {
            self.0.lock().unwrap().push(sql);
        }
    }
}

#[tokio::test]
async fn million_row_adapter_uses_bounded_day_ranges_and_indexed_earliest_lookup() {
    let statements = Statements::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(statements.clone()),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scale.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(concat!(
                                 "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u','user','User',1,1),('v','viewer','Viewer',1,1); ",
                                 seed_direct_pair!(
                                     id = "private",
                                     endpoint_a_kind = "human",
                                     endpoint_a_id = "u",
                                     endpoint_b_kind = "human",
                                     endpoint_b_id = "v",
                                 ),
                                 "; ",
                                 "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) SELECT 'm'||x,'private','user',CASE WHEN x%100=0 THEN 'u' ELSE 'v' END,'',x,1672502400000+(x%1096)*86400000 FROM n; ",
                             ) )).await.unwrap();
    let year = MessageCountYear::new(2024).unwrap();
    let store = MessagingStore::open(&path).await.unwrap();
    for subject in [
        Principal::human(UserId::parse("u").unwrap()),
        Principal::agent(AgentId::parse("u").unwrap()),
    ] {
        statements.0.lock().unwrap().clear();
        let result = store.read_counts(&subject, &year).await.unwrap();
        let queries = std::mem::take(&mut *statements.0.lock().unwrap());
        assert_eq!(
            queries.len(),
            2,
            "must capture both completed adapter reads: {queries:?}"
        );
        if matches!(subject, Principal::Human { .. }) {
            // Fixture has 10,000 u rows, cycling through 1,096 days in steps of 100.
            // Exactly 3,321 fall in Shanghai's 2024 interval; earliest is 2023-01-01.
            assert_eq!(result.days.len(), 91);
            assert_eq!(result.days.iter().map(|v| v.1).sum::<i64>(), 3321);
            assert_eq!(result.earliest_ms, Some(1_672_502_400_000));
        } else {
            assert!(result.days.is_empty());
            assert_eq!(result.earliest_ms, None);
        }
        for sql in queries {
            let explain = format!("EXPLAIN QUERY PLAN {sql}");
            let daily = sql.contains("json_each");
            // Only SQL captured from this adapter; no user input is interpolated.
            let mut query =
                sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(explain.as_str()));
            if daily {
                query = query.bind(serde_json::to_string(&year.day_bounds).unwrap());
            }
            let rows = query.bind("u").fetch_all(&mut db).await.unwrap();
            let plan = rows
                .iter()
                .map(|r| r.3.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            assert!(plan.contains("SEARCH"), "{plan}");
            assert!(
                !plan.contains("SCAN messages") && !plan.contains("SCAN m "),
                "{plan}"
            );
            if daily {
                assert!(plan.contains("created_at>? AND created_at<?"), "{plan}");
            }
        }
    }
    db.close().await.unwrap();
}
