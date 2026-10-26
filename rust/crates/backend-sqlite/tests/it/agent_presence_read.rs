//! Integration tests for the agent-presence cold-read SQL (#task:2195, batch 4).
//!
//! The route-level tests (backend-server/src/host_runtime/agent_presence.rs) and the
//! composed contract (backend-server/tests/agent_presence_contract.rs) pin the ACL
//! ladder and the wire projection against real rows. These pin the ONE thing neither
//! covers: that the adapter STRICT-DECODES the stored `last_activity_at` BEFORE it
//! runs the ACL arms or projects anything, so a committed value outside the usable
//! range surfaces as a named, source-bearing read error (a safe 500 upstream) rather
//! than being saturated into a legal deadline.
//!
//! Three corruption→repair discriminators exercise the decode gate on ONE open adapter
//! over ONE database: the single stamp is corrupted and then repaired IN PLACE and
//! re-read, so a decode fault that stuck as sticky pool/cache state ("first bad, then
//! permanently bad") or a stale cached read that masked a repair would be caught — a
//! guarantee a pair of fresh-database reads cannot make. The third discriminator is
//! load-bearing: a stamp that is itself JS-safe but whose away deadline would overflow
//! must still be refused, proving the bound is the DEADLINE, not the raw stamp range.

use silverhand_backend_core::{
    foundation::identity::AgentId,
    foundation::persistence::PersistenceFailure,
    host_runtime::agent_presence::{self, ActivityInstant},
    host_runtime::agent_presence_read::{AgentPresenceFacts, AgentPresenceReadModel},
};
use silverhand_backend_sqlite::host_runtime::agent_presence_read::SqliteAgentPresenceColdRead;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

use crate::current_schema;

const CREATOR: &str = "INSERT INTO users (id, name, display_name, created_at, updated_at) \
     VALUES ('u-creator', 'creator', 'Creator', 1000, 1000)";
const HOST_H1: &str =
    "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'host-one', 'desktop', 1000)";
const AGENT_WITH_STAMP: &str = "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
     created_by_user_id, materialization_status, last_activity_at) \
     VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator', 'active', ?)";

/// The database file name inside the temp dir, shared so an in-place updater can reopen
/// the exact file the adapter holds open.
const DB_FILE: &str = "agent-presence-read.sqlite";

fn agent(id: &str) -> AgentId {
    AgentId::parse(id).expect("agent id")
}

/// Create a fresh current-schema database, seed the creator/host, commit the agent row
/// with a chosen `last_activity_at` (`None` → `NULL`, or any number including a
/// deliberately corrupt one, bound so the SQL stays static), and open the adapter over
/// it. The `TempDir` guard outlives the pool.
async fn seeded(stamp: Option<i64>) -> (tempfile::TempDir, SqliteAgentPresenceColdRead) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(DB_FILE);
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in [CREATOR, HOST_H1] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    sqlx::query(AGENT_WITH_STAMP)
        .bind(stamp)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let cold_read = SqliteAgentPresenceColdRead::open(&path).await.unwrap();
    (directory, cold_read)
}

/// Overwrite `a1`'s committed `last_activity_at` in place, on a FRESH connection to the
/// same file the adapter holds open, so the adapter's next read observes the new
/// committed value rather than a fixture built on a different database.
async fn update_stamp(path: &std::path::Path, stamp: i64) {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE agents SET last_activity_at = ? WHERE id = 'a1'")
        .bind(stamp)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
}

/// The fallible read, so a falsifier can assert a corrupt committed stamp surfaces as
/// a read error rather than being coerced to a saturated deadline.
async fn try_presence(
    cold_read: &SqliteAgentPresenceColdRead,
    id: &str,
) -> Result<Option<AgentPresenceFacts>, PersistenceFailure> {
    cold_read.read_agent_presence(&agent(id)).await
}

async fn presence_of(cold_read: &SqliteAgentPresenceColdRead, id: &str) -> AgentPresenceFacts {
    try_presence(cold_read, id)
        .await
        .unwrap()
        .expect("agent present")
}

/// The chain of `Display` messages, so a falsifier can prove the error is the NAMED
/// stamp corruption (carrying the offending value and column), not some other fault.
fn source_chain(error: &PersistenceFailure) -> String {
    let mut chain = String::new();
    let mut current: Option<&dyn std::error::Error> = Some(error);
    while let Some(link) = current {
        chain.push_str(&link.to_string());
        chain.push('\n');
        current = link.source();
    }
    chain
}

// --- Positive controls: the range the adapter accepts. -----------------------------

#[tokio::test]
async fn a_valid_stamp_projects_the_shared_away_deadline() {
    let (_guard, cold_read) = seeded(Some(1_000)).await;
    let facts = presence_of(&cold_read, "a1").await;
    assert_eq!(facts.source(), &agent("a1"));
    assert_eq!(facts.away_at(), Some(1_000 + agent_presence::AWAY_TTL_MS));
}

#[tokio::test]
async fn a_null_stamp_reads_as_no_deadline() {
    let (_guard, cold_read) = seeded(None).await;
    assert_eq!(presence_of(&cold_read, "a1").await.away_at(), None);
}

/// The largest ACCEPTED stamp round-trips through the adapter and deadlines exactly on
/// the wire ceiling — the point of pinning the domain to the deadline.
#[tokio::test]
async fn the_max_acceptable_stamp_deadlines_on_the_wire_ceiling() {
    let (_guard, cold_read) = seeded(Some(ActivityInstant::MAX)).await;
    assert_eq!(
        presence_of(&cold_read, "a1").await.away_at(),
        Some(agent_presence::MAX_SAFE_INTEGER)
    );
}

// --- Strict-decode falsifiers: a corrupt committed stamp is a read error. -----------

#[tokio::test]
async fn corrupting_and_repairing_the_stamp_in_place_flips_the_same_adapter_read() {
    let (guard, cold_read) = seeded(Some(1_000)).await;
    let path = guard.path().join(DB_FILE);
    let max_safe = agent_presence::MAX_SAFE_INTEGER;
    // The corrupt values, table-driven. The third is load-bearing: the raw stamp is
    // itself JS-safe (<= MAX_SAFE_INTEGER), only its away deadline overflows, so a
    // raw-range check alone would wrongly accept it.
    let discriminators: [(&str, i64); 3] = [
        ("negative", -1),
        ("beyond the wire ceiling", max_safe + 1),
        (
            "wire-safe stamp with an overflowing deadline",
            ActivityInstant::MAX + 1,
        ),
    ];

    for (label, corrupt) in discriminators {
        // A clean read on this same adapter first: proves the fault below is THIS
        // corruption, not sticky state left by the previous discriminator's repair.
        assert_eq!(
            try_presence(&cold_read, "a1")
                .await
                .expect("a repaired stamp reads cleanly")
                .expect("agent present")
                .away_at(),
            Some(1_000 + agent_presence::AWAY_TTL_MS),
            "the adapter must read cleanly before the {label} corruption"
        );

        // Corrupt the single committed stamp IN PLACE, then read through the SAME
        // adapter: the decode gate must fault rather than saturate into a deadline.
        update_stamp(&path, corrupt).await;
        let error = try_presence(&cold_read, "a1")
            .await
            .expect_err(&format!("a {label} stamp must surface as a read error"));
        let chain = source_chain(&error);
        assert!(
            chain.contains("last_activity_at"),
            "the {label} error must name the offending column: {chain}"
        );
        assert!(
            chain.contains(&corrupt.to_string()),
            "the {label} error must carry the offending value: {chain}"
        );

        // Repair the SAME row in place; the very NEXT read on the SAME adapter must
        // recover, proving the fault left no "first bad, then permanently bad" state
        // and no cached bad value.
        update_stamp(&path, 1_000).await;
        assert_eq!(
            presence_of(&cold_read, "a1").await.away_at(),
            Some(1_000 + agent_presence::AWAY_TTL_MS),
            "repairing the {label} stamp in place must let the SAME adapter read succeed"
        );
    }
}

#[tokio::test]
async fn an_absent_agent_reads_as_no_presence() {
    let (_guard, cold_read) = seeded(Some(1_000)).await;
    assert_eq!(try_presence(&cold_read, "a-missing").await.unwrap(), None);
}
