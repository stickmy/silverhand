//! The falsifier for the LIST read's BOUNDED audience (#task:2195, batch 5, P0).
//!
//! The batch's thesis is that the visibility ACL is computed in a fixed number of SQL
//! statements, not once per candidate. A `1 + 2N` per-candidate audience read satisfies
//! every behavioural test in `agent_list_read.rs` — the facts it builds are identical —
//! so nothing there can tell a bounded read from an unbounded one. This test can: it
//! COUNTS the read statements the adapter issues and pins that the count does not grow
//! with the candidate count.
//!
//! It lives in its own test binary because it installs a PROCESS-WIDE tracing subscriber
//! (`set_global_default`) to catch sqlx's per-statement `sqlx::query` events — sqlx runs
//! SQLite on a worker thread, so a thread-local subscriber would miss them, and the
//! global default may be set only once per process. The subscriber is hand-rolled over
//! `tracing` alone (a normal dependency), so this adds no dev-dependency and leaves the
//! frozen lockfile untouched.
//!
//! It counts only statements whose summary begins with `SELECT`: those are the adapter's
//! READS. Connection-setup `PRAGMA`s fire on whichever measured read the pool happens to
//! open a fresh connection on, so counting them would be pool noise, not read work; sqlx
//! does not log `BEGIN`/`COMMIT` as query events at all. The self-check
//! (`reads_for_one > 0`) makes a broken counter fail LOUD rather than pass green: if sqlx
//! emitted nothing we would otherwise "prove" boundedness by counting zero against zero.

#[macro_use]
#[path = "support/seed_rows.rs"]
mod seed_rows;

use silverhand_backend_core::{
    foundation::identity::UserId, identity_access::agent_list_read::AgentListReadModel,
};
use silverhand_backend_sqlite::identity_access::agent_list_read::SqliteAgentListColdRead;
use sqlx::Connection;

#[path = "support/current_schema_snapshot.rs"]
mod current_schema;

#[path = "support/statement_log.rs"]
mod statement_log;
use statement_log::StatementLog;

fn user(id: &str) -> UserId {
    UserId::parse(id).expect("user id")
}

// A tiny fixture using the CREATOR arm only, so no relation rows are needed: `u-solo`
// created one agent, `u-many` created three. That is all the differential needs — one
// viewer with strictly more candidates than the other — while the bounded batch still
// issues its two (empty) arm reads for both.
const FIXTURE: &[&str] = &[
    "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES \
       ('u-solo', 'solo', 'Solo', 1, 1), \
       ('u-many', 'many', 'Many', 1, 1)",
    "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'host-one', 'desktop', 1)",
    seed_agent!(
        id = "a-solo",
        name = "agent-solo",
        runtime = "qoder",
        host_id = "h1",
        created_at = 100,
        updated_at = 100,
        created_by_user_id = "u-solo",
    ),
    seed_agent!(
        id = "a-m1",
        name = "agent-m1",
        runtime = "qoder",
        host_id = "h1",
        created_at = 200,
        updated_at = 200,
        created_by_user_id = "u-many",
    ),
    seed_agent!(
        id = "a-m2",
        name = "agent-m2",
        runtime = "qoder",
        host_id = "h1",
        created_at = 300,
        updated_at = 300,
        created_by_user_id = "u-many",
    ),
    seed_agent!(
        id = "a-m3",
        name = "agent-m3",
        runtime = "qoder",
        host_id = "h1",
        created_at = 400,
        updated_at = 400,
        created_by_user_id = "u-many",
    ),
];

#[tokio::test]
async fn the_list_read_issues_a_constant_number_of_read_statements() {
    let log = StatementLog::default();
    tracing::subscriber::set_global_default(log.clone())
        .expect("the only test in this binary owns the process-wide default");

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent-list-statement-count.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in FIXTURE {
        sqlx::query(*statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();

    let list = SqliteAgentListColdRead::open(&path).await.unwrap();

    // Warm the pool so a connection is established before either measured read.
    list.read_agent_candidates(&user("u-many")).await.unwrap();

    let before_one = log.len();
    let one = list.read_agent_candidates(&user("u-solo")).await.unwrap();
    let reads_for_one = log.reads_since(before_one);

    let before_many = log.len();
    let many = list.read_agent_candidates(&user("u-many")).await.unwrap();
    let reads_for_many = log.reads_since(before_many);

    // The two reads must genuinely differ in candidate count, or the differential is
    // vacuous.
    assert_eq!(
        one.len(),
        1,
        "u-solo should see exactly its one created agent"
    );
    assert!(
        many.len() > one.len(),
        "u-many ({}) must see strictly more candidates than u-solo ({}) for the \
         differential to bite",
        many.len(),
        one.len()
    );

    // Self-check: no reads captured means the subscriber never saw sqlx's events, so a
    // passing equality below would be zero-against-zero — a false green. Fail loud.
    assert!(
        !reads_for_one.is_empty(),
        "no sqlx::query read events were captured; the counter is broken, not a pass"
    );

    // The load-bearing assertion. A per-candidate audience read is `1 + 2N` SELECTs, so
    // its count would grow from u-solo to u-many; the bounded set-shaped read is a
    // constant three (candidate rows, then one statement per arm).
    assert_eq!(
        reads_for_one.len(),
        reads_for_many.len(),
        "read-statement count grew with candidate count ({} for one candidate vs {} for \
         {}); the audience read is not bounded.\n  one:  {reads_for_one:#?}\n  many: \
         {reads_for_many:#?}",
        reads_for_one.len(),
        reads_for_many.len(),
        many.len()
    );
}
