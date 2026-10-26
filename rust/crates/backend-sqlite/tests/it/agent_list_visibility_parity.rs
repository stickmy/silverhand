//! Rust half of the cross-language `GET /api/agents` list set-signer (#task:2195,
//! batch 5).
//!
//! The SAME checked-in fixture the TypeScript oracle ratifies
//! (packages/backend/tests/read-model/agent-list-visibility-rust-parity.test.ts) is
//! replayed here, but as a STRUCTURED DOMAIN seed rather than an executable SQL
//! script. The fixture names domain entities (humans, agents with a
//! creator/created_at/stage, direct pairs, shared workspaces) with symbolic refs;
//! each side resolves those refs to its own concrete ids. On this side the refs ARE
//! the ids, so this test translates the structured facts into current-schema INSERTs
//! and replays them against a fresh database, then asserts the ordered visible-agent
//! ref set per case. The TS half builds the same facts through the production
//! repository builders and signs the expected sets against the real `agents.list()`
//! filtered by `canViewAgent` before this side is asked to match — so this is a
//! pure differential against a ratified artifact, over ONE shared set of product
//! facts.
//!
//! The value is produced by the REAL production stack this batch ports: the SQLite
//! `SqliteAgentListColdRead::read_agent_candidates` adapter (its bounded, set-shaped
//! audience read on one deferred snapshot, its `created_at ASC` order carried
//! verbatim) feeding the core `visible_agents` projector (the ONE `can_view_agent`
//! authority re-run per row, the `?scope=owned` narrowing by ACTUAL creator, the
//! stage widener).
//!
//! It is a SET signer: the ordered visible-id set is the load-bearing signal of this
//! vertical. The per-agent DTO projection is signed by the `GET /api/agents/:id`
//! gate and is not re-checked here.

use silverhand_backend_core::{
    foundation::identity::UserId,
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agent_list_read::{AgentListReadModel, ListScope, visible_agents},
    identity_access::capability::agent_view::VisibleStages,
};
use silverhand_backend_sqlite::identity_access::agent_list_read::SqliteAgentListColdRead;
use sqlx::{AssertSqlSafe, Connection, Executor};

use crate::current_schema;

const FIXTURE: &str =
    include_str!("../../../../../packages/shared/tests/fixtures/agent-list-visibility.json");

#[derive(serde::Deserialize)]
struct Fixture {
    humans: Vec<String>,
    agents: Vec<AgentSeed>,
    direct_pairs: Vec<DirectPairSeed>,
    workspaces: Vec<WorkspaceSeed>,
    cases: Vec<Case>,
}

#[derive(serde::Deserialize)]
struct AgentSeed {
    #[serde(rename = "ref")]
    reference: String,
    creator: String,
    created_at: i64,
    status: String,
}

#[derive(serde::Deserialize)]
struct DirectPairSeed {
    agent: String,
    human: String,
}

#[derive(serde::Deserialize)]
struct WorkspaceSeed {
    #[serde(rename = "ref")]
    reference: String,
    agent_members: Vec<String>,
    human_users: Vec<String>,
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    viewer: String,
    scope: String,
    include_failed: bool,
    include_pending: bool,
    expected_agent_ids: Vec<String>,
}

impl Case {
    /// The stage set the flags widen to — the same assembly the route performs, but
    /// consumed here as the parsed intent the cold read takes directly.
    fn stages(&self) -> VisibleStages {
        let mut stages = vec![MaterializationStatus::Active];
        if self.include_failed {
            stages.push(MaterializationStatus::Failed);
        }
        if self.include_pending {
            stages.push(MaterializationStatus::PendingMaterialization);
        }
        VisibleStages::including(stages)
    }

    fn list_scope(&self) -> ListScope {
        match self.scope.as_str() {
            "owned" => ListScope::Owned,
            "all" => ListScope::All,
            other => panic!("case {}: unknown scope {other:?}", self.name),
        }
    }
}

/// The fixture's stage word → the `materialization_status` column value.
fn status_sql(status: &str) -> &'static str {
    match status {
        "active" => "active",
        "failed" => "failed",
        "pending" => "pending_materialization",
        other => panic!("unknown agent status {other:?}"),
    }
}

/// A single-quoted SQL string literal with embedded quotes doubled. The refs are
/// controlled fixture identifiers, but escaping keeps the translation honest.
fn sql_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Translate the structured domain seed into the current-schema INSERTs that realise
/// it. Refs are the literal ids on this side; the direct_pair row mirrors the
/// production `getOrCreateDirectPair` shape (agent in endpoint_a, human in endpoint_b).
fn build_seed(fixture: &Fixture) -> String {
    let mut stmts: Vec<String> = vec![
        "INSERT INTO hosts (id, name, kind, created_at) VALUES ('h1', 'host-one', 'desktop', 1)"
            .to_string(),
    ];

    for human in &fixture.humans {
        stmts.push(format!(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ({h}, {h}, {h}, 1, 1)",
            h = sql_str(human),
        ));
    }

    for agent in &fixture.agents {
        stmts.push(format!(
            "INSERT INTO agents (id, name, display_name, runtime, host_id, created_at, updated_at, created_by_user_id, materialization_status) \
             VALUES ({id}, {name}, {name}, 'qoder', 'h1', {created_at}, {created_at}, {creator}, {status})",
            id = sql_str(&agent.reference),
            name = sql_str(&format!("agent-{}", agent.reference)),
            created_at = agent.created_at,
            creator = sql_str(&agent.creator),
            status = sql_str(status_sql(&agent.status)),
        ));
    }

    for pair in &fixture.direct_pairs {
        stmts.push(format!(
            "INSERT INTO conversations (id, workspace_id, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, placement_kind, roster_policy, title, status, created_at, updated_at) \
             VALUES ({id}, NULL, 'agent', {agent}, 'human', {human}, 'direct_pair', 'fixed', 'dm', 'active', 1, 1)",
            id = sql_str(&format!("c-{}", pair.agent)),
            agent = sql_str(&pair.agent),
            human = sql_str(&pair.human),
        ));
    }

    for workspace in &fixture.workspaces {
        stmts.push(format!(
            "INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, archived_at, created_at, updated_at) \
             VALUES ({id}, {id}, {id}, NULL, NULL, NULL, 1, 1)",
            id = sql_str(&workspace.reference),
        ));
        for agent in &workspace.agent_members {
            stmts.push(format!(
                "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES ({ws}, {agent}, 1)",
                ws = sql_str(&workspace.reference),
                agent = sql_str(agent),
            ));
        }
        for human in &workspace.human_users {
            stmts.push(format!(
                "INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ({ws}, {user}, 1)",
                ws = sql_str(&workspace.reference),
                user = sql_str(human),
            ));
        }
    }

    format!("{};", stmts.join(";\n"))
}

#[tokio::test]
async fn the_list_vertical_reproduces_every_signed_visibility_case() {
    let fixture: Fixture = serde_json::from_str(FIXTURE).expect("parse shared fixture");

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent-list-visibility.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    let seed = build_seed(&fixture);
    db.execute(sqlx::raw_sql(AssertSqlSafe(seed)))
        .await
        .expect("structured list seed must load against the current schema");
    db.close().await.unwrap();

    let list = SqliteAgentListColdRead::open(&path).await.unwrap();

    for case in &fixture.cases {
        let viewer = UserId::parse(&case.viewer).expect("case viewer id");
        let candidates = list
            .read_agent_candidates(&viewer)
            .await
            .expect("candidate read");
        let visible = visible_agents(&viewer, &candidates, &case.stages(), case.list_scope())
            .expect("no narrow/decide drift in a signed fixture");
        let ids: Vec<String> = visible
            .iter()
            .map(|facts| facts.source().as_str().to_owned())
            .collect();

        assert_eq!(ids, case.expected_agent_ids, "case {}", case.name);
    }
}
