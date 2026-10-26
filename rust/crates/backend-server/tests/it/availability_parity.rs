//! Rust half of the cross-language GET /api/agents/{id}/availability whole-wire
//! parity gate (#task:2195, batch 2).
//!
//! The checked-in fixtures ratified by the now-retired TypeScript oracle
//! (packages/backend/tests/availability-cold-read-parity.test.ts) are replayed
//! here: one `seed.sql` executed verbatim against a fresh current-schema database,
//! one `input.json` naming the authenticated viewer, the agent asked about, and
//! the hosts the delivery transport is attached to, and one `expected.json` —
//! status AND body — the wire response must equal. The value is produced by the
//! REAL production stack: the SQLite `SqliteAgentAvailabilityColdRead` adapter
//! (its three ACL arms on one deferred snapshot, its strict decode) driven through
//! the real `host_runtime::availability::router`, so the actual wire projector — the
//! ACL-first two-authority order, the `{"availability": ...}` success body, and the
//! one anti-enumeration `404 {"error":"agent not found"}` an absent row, a refused
//! viewer, and a hidden stage all collapse to — is what gets compared.
//!
//! The expected JSON was hand-authored and signed by the real TS authority
//! during migration. This test compares Rust against that frozen artifact;
//! it does not run a live TS oracle. The route is
//! authenticated, so the request injects the `UserId` extension the composition
//! perimeter would attach; the auth-refusal body is the perimeter's (#task:2197),
//! not this gate's.

use std::collections::HashSet;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use silverhand_backend_core::{
    foundation::identity::{HostId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::availability::HostAttachmentFacts,
};
use silverhand_backend_server::host_runtime::{
    availability::router, host_attachment::HostAttachment,
};
use silverhand_backend_sqlite::host_runtime::agent_availability_read::SqliteAgentAvailabilityColdRead;
use sqlx::{Connection, Executor};
use tower::ServiceExt;

use crate::current_schema;

/// The fixture's attachment oracle: exactly the hosts named in `attached_hosts`
/// read attached, mirroring the TypeScript `connectionIdForHost` non-null arm.
#[derive(Clone)]
struct FixedAttachment(HashSet<String>);

impl HostAttachment for FixedAttachment {
    fn host_attachment(&self, host: &HostId) -> Result<HostAttachmentFacts, PersistenceFailure> {
        Ok(HostAttachmentFacts {
            host_id: host.clone(),
            attached: self.0.contains(host.as_str()),
        })
    }
}

/// Every fixture is three sibling files under one case directory; bundling their
/// `include_str!`s keeps a renamed or missing file a compile error, not a skipped
/// case.
macro_rules! fixture {
    ($name:literal) => {{
        const SEED: &str = include_str!(concat!(
            "../../../../../rust/test-fixtures/fixtures/availability/",
            $name,
            "/seed.sql"
        ));
        const INPUT: &str = include_str!(concat!(
            "../../../../../rust/test-fixtures/fixtures/availability/",
            $name,
            "/input.json"
        ));
        const EXPECTED: &str = include_str!(concat!(
            "../../../../../rust/test-fixtures/fixtures/availability/",
            $name,
            "/expected.json"
        ));
        (SEED, INPUT, EXPECTED)
    }};
}

async fn run_fixture(seed_sql: &'static str, input_json: &str, expected_json: &str) {
    let input: Value = serde_json::from_str(input_json).unwrap();
    let expected: Value = serde_json::from_str(expected_json).unwrap();
    let viewer = input["viewer"].as_str().expect("input.viewer");
    let agent_id = input["agent_id"].as_str().expect("input.agent_id");
    let attached: HashSet<String> = input["attached_hosts"]
        .as_array()
        .expect("input.attached_hosts")
        .iter()
        .map(|value| value.as_str().expect("attached host id").to_owned())
        .collect();
    let expected_status = u16::try_from(expected["status"].as_u64().expect("expected.status"))
        .expect("status in range");
    let expected_body = expected["body"].clone();

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("availability-parity.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(seed_sql))
        .await
        .expect("shared availability seed must load against the current schema");
    db.close().await.unwrap();

    let cold_read = SqliteAgentAvailabilityColdRead::open(&path).await.unwrap();
    let app: Router = router(cold_read, FixedAttachment(attached));

    // The authenticated principal is the composition perimeter's contribution;
    // this test isolates the production projector, so it injects the viewer fact
    // the auth layer would otherwise attach.
    let request = Request::builder()
        .uri(format!("/api/agents/{agent_id}/availability"))
        .extension(UserId::parse(viewer).unwrap())
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::from_u16(expected_status).unwrap()
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
    );

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let actual: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(actual, expected_body);
}

macro_rules! parity_case {
    ($test:ident, $name:literal) => {
        #[tokio::test]
        async fn $test() {
            let (seed, input, expected) = fixture!($name);
            run_fixture(seed, input, expected).await;
        }
    };
}

parity_case!(online_creator, "online-creator");
parity_case!(online_direct_pair, "online-direct-pair");
parity_case!(online_workspace_member, "online-workspace-member");
parity_case!(online_agent_home, "online-agent-home");
parity_case!(offline_deleted, "offline-deleted");
parity_case!(offline_revoked, "offline-revoked");
parity_case!(offline_detached, "offline-detached");
parity_case!(deny_not_viewer, "deny-not-viewer");
parity_case!(miss_absent_agent, "miss-absent-agent");
parity_case!(stage_hidden, "stage-hidden");
