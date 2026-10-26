//! Rust half of the cross-language GET /api/presence whole-wire parity gate
//! (#task:2195, batch 1 P0-2).
//!
//! The checked-in fixtures ratified by the now-retired TypeScript oracle
//! (packages/backend/tests/presence-cold-read-parity.test.ts) are replayed here:
//! one `seed.sql` executed verbatim against a fresh current-schema database, one
//! `online.json` naming the present viewers, and one `expected.json` the wire body
//! must equal. Here the value is produced by the REAL production stack — the SQLite
//! `SqliteUserPresenceColdRead` adapter, sealed into a `PresenceSnapshot`, driven
//! through the real `viewer_read_model::user_presence_http::router` so the actual wire projector
//! (snake_case, the lowercase closed enum, the online arm's suppressed `null`, the
//! `(created_at, id)` order) is what gets compared.
//!
//! The expected JSON was hand-authored and signed by the real TS authority
//! during migration. Rust must preserve that frozen contract for discrimination,
//! listed order, and per-viewer stamp suppression; no live TS oracle runs here.
//! The route is authenticated, so the request injects the
//! `UserId` extension the composition perimeter would attach; the auth-refusal body
//! is the perimeter's (#task:2197), not this gate's.

use std::collections::HashSet;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use silverhand_backend_core::{
    foundation::identity::UserId, viewer_read_model::presence::OnlineViewers,
};
use silverhand_backend_server::viewer_read_model::user_presence_http::{PRESENCE_PATH, router};
use silverhand_backend_sqlite::viewer_read_model::user_presence_cold_read::SqliteUserPresenceColdRead;
use sqlx::{Connection, Executor};
use tower::ServiceExt;

use crate::current_schema;

const SUCCESS_SEED: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/presence/success/seed.sql");
const SUCCESS_ONLINE: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/presence/success/online.json");
const SUCCESS_EXPECTED: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/presence/success/expected.json");
const EMPTY_SEED: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/presence/empty/seed.sql");
const EMPTY_ONLINE: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/presence/empty/online.json");
const EMPTY_EXPECTED: &str =
    include_str!("../../../../../rust/test-fixtures/fixtures/presence/empty/expected.json");

/// The fixture's liveness oracle: exactly the ids named in `online.json` read
/// present, mirroring the TypeScript `ctx.presence.isOnline` the oracle drives.
#[derive(Clone)]
struct FixedOnline(HashSet<UserId>);

impl OnlineViewers for FixedOnline {
    fn is_online(&self, user_id: &UserId) -> bool {
        self.0.contains(user_id)
    }
}

async fn run_fixture(seed_sql: &'static str, online_json: &str, expected_json: &str) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("presence-parity.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(seed_sql))
        .await
        .expect("shared presence seed must load against the current schema");
    db.close().await.unwrap();

    let cold_read = SqliteUserPresenceColdRead::open(&path).await.unwrap();
    let online = FixedOnline(
        serde_json::from_str::<Vec<String>>(online_json)
            .unwrap()
            .into_iter()
            .map(|id| UserId::parse(id).unwrap())
            .collect(),
    );
    let app: Router = router(cold_read, online);

    // The authenticated principal is the composition perimeter's contribution;
    // this test isolates the production projector, so it injects the viewer fact
    // the auth layer would otherwise attach.
    let request = Request::builder()
        .uri(PRESENCE_PATH)
        .extension(UserId::parse("viewer-1").unwrap())
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
    );

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let actual: Value = serde_json::from_slice(&body).unwrap();
    let expected: Value = serde_json::from_str(expected_json).unwrap();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn the_real_presence_projector_produces_the_shared_success_body() {
    run_fixture(SUCCESS_SEED, SUCCESS_ONLINE, SUCCESS_EXPECTED).await;
}

#[tokio::test]
async fn the_real_presence_projector_produces_the_shared_empty_body() {
    run_fixture(EMPTY_SEED, EMPTY_ONLINE, EMPTY_EXPECTED).await;
}
