//! The SQL authority for the agent-visibility ACL: ONE tagged relation.
//!
//! The stored relation is `(agent_id, user_id, arm)` with
//! `arm ∈ {direct_pair, workspace}`. Every question the product asks of this ACL is
//! that one relation read from an end:
//!
//! * the AUDIENCE of an agent — who may see it — is `WHERE agent_id = ?`, taken one
//!   arm at a time so each fact source still supplies
//!   `AgentViewerArms::new` its arms separately rather than handing it a pre-merged
//!   blob;
//! * the CANDIDATES of a viewer — which agents it may see — is `WHERE user_id = ?`,
//!   and needs no partition, because a candidate is admitted by ANY arm.
//!
//! `GET /api/agents/{id}/availability`, `GET /api/agents/{id}`,
//! `GET /api/agents/{id}/presence` and the `GET /api/agents` list face all decide
//! the same "who may see this agent" question. Two expressions of one membership
//! rule drift, and a drift in an ACL is a security bug, not a style nit — so the
//! membership text exists once and both directions are derived from it. These are
//! the ports of the TypeScript `DIRECT_PAIR_HUMAN_ENDPOINT_IDS_FOR_AGENT_SQL` and
//! the `workspaces.listForAgent` ⇒ `workspaceMemberUserIds` pair.
//!
//! ## What the arm tag does and does not buy
//!
//! It buys a single relation that still supplies two named arms, and one spelling
//! per arm: the SQL literal in the relation and the value bound to the filter come
//! from the SAME token, so an arm cannot be renamed on one side only.
//!
//! It does NOT prove which legal branch a row came from. `AgentViewerArms` unions
//! its arms immediately, so swapping `direct_pair` for `workspace` on a row is
//! invisible to every product judgement downstream. Arm attribution is therefore
//! pinned HERE, by per-direction reads over a fixture built so each arm holds a
//! user the other does not; a test that can only observe the union cannot stand in
//! for that proof.
//!
//! ## The creator arm is not here
//!
//! It is a single column on the agent row, so it needs no query and no relation
//! membership. Keeping it out also keeps it honest: the creator arm drives the ACL,
//! `scope=owned` and the wire's `created_by_user_id` from the one column the read
//! actually selected.

use std::collections::HashMap;

use silverhand_backend_core::foundation::identity::{AgentId, UserId};
use sqlx::{Row, SqliteConnection};

macro_rules! direct_pair_arm_tag {
    () => {
        "direct_pair"
    };
}
pub(crate) use direct_pair_arm_tag;

macro_rules! workspace_arm_tag {
    () => {
        "workspace"
    };
}
pub(crate) use workspace_arm_tag;

/// The membership text. A SET (`UNION`, never `UNION ALL`), so a derived read needs
/// no `DISTINCT` of its own: one `(agent_id, user_id, arm)` triple appears once
/// however many rows witness it.
///
/// The direct-pair arm covers both slot orders with the OTHER endpoint gated on
/// `'human'`, so an agent↔agent pair contributes nobody. The workspace arm is every
/// workspace an agent reaches — the regular workspaces it holds a
/// `workspace_members` row in, plus its own agent-home workspace
/// (`owner_agent_id`) — joined to that workspace's users. For a human, workspace
/// visibility IS membership, so the arm is exact and not an over-approximation.
macro_rules! agent_viewer_relation {
    () => {
        concat!(
            "SELECT endpoint_a_id AS agent_id, endpoint_b_id AS user_id, '",
            $crate::identity_access::agent_view_acl::direct_pair_arm_tag!(),
            "' AS arm \
               FROM conversations \
              WHERE placement_kind = 'direct_pair' \
                AND endpoint_a_kind = 'agent' \
                AND endpoint_b_kind = 'human' \
             UNION \
            SELECT endpoint_b_id AS agent_id, endpoint_a_id AS user_id, '",
            $crate::identity_access::agent_view_acl::direct_pair_arm_tag!(),
            "' AS arm \
               FROM conversations \
              WHERE placement_kind = 'direct_pair' \
                AND endpoint_b_kind = 'agent' \
                AND endpoint_a_kind = 'human' \
             UNION \
            SELECT reach.agent_id AS agent_id, wu.user_id AS user_id, '",
            $crate::identity_access::agent_view_acl::workspace_arm_tag!(),
            "' AS arm \
               FROM ( \
                      SELECT wm.agent_id AS agent_id, w.id AS workspace_id \
                        FROM workspaces w \
                        JOIN workspace_members wm ON wm.workspace_id = w.id \
                       WHERE w.owner_agent_id IS NULL \
                      UNION \
                      SELECT w.owner_agent_id AS agent_id, w.id AS workspace_id \
                        FROM workspaces w \
                       WHERE w.owner_agent_id IS NOT NULL \
                    ) reach \
               JOIN workspace_users wu ON wu.workspace_id = reach.workspace_id"
        )
    };
}
pub(crate) use agent_viewer_relation;

/// One arm of one agent's audience. The arm travels as a BOUND value, not as text
/// spliced into the query.
const SELECT_AUDIENCE_ARM: &str = concat!(
    "SELECT rel.user_id AS user_id FROM (",
    agent_viewer_relation!(),
    ") rel WHERE rel.agent_id = ? AND rel.arm = ?"
);

/// The LIST direction of the SAME relation: which agents a given viewer may see,
/// bound by `user_id`. It needs no per-arm partition — a candidate is admitted by
/// ANY arm — and `DISTINCT` collapses an agent reached through several arms to one
/// id. This narrows to CANDIDATES only: `agent_list_read::visible_agents` re-runs
/// the capability per row, so this text may never DECIDE, only offer a faithful
/// superset. The creator arm is NOT here (it is a column on the agent row), so the
/// list adapter unions this with the creator predicate itself.
///
/// A `macro_rules!` fragment so the list adapter can splice it into its row query
/// through `concat!` at compile time (sqlx accepts only a `&'static str`); the
/// `const` below is the same text, kept for the direct-read test.
macro_rules! select_viewer_candidate_ids {
    () => {
        concat!(
            "SELECT DISTINCT rel.agent_id AS agent_id FROM (",
            $crate::identity_access::agent_view_acl::agent_viewer_relation!(),
            ") rel WHERE rel.user_id = ?"
        )
    };
}
pub(crate) use select_viewer_candidate_ids;

/// The candidate-set membership predicate over an `agents a` row: an agent a viewer
/// may see is one it CREATED (a plain column) OR one the relation admits it to. Spelled
/// ONCE and shared verbatim by the list row query and the bounded audience batch below,
/// so "which agents are a viewer's candidates" cannot drift between the two reads. Binds,
/// in order: the viewer id for the creator column, then the viewer id for the relation
/// candidate subquery.
macro_rules! viewer_candidate_predicate {
    () => {
        concat!(
            "a.created_by_user_id = ? OR a.id IN (",
            $crate::identity_access::agent_view_acl::select_viewer_candidate_ids!(),
            ")"
        )
    };
}
pub(crate) use viewer_candidate_predicate;

/// One arm of the audience for a viewer's WHOLE candidate set, in a single statement.
///
/// This is [`SELECT_AUDIENCE_ARM`] widened from one agent to the candidate set: it still
/// partitions by `WHERE rel.arm = ?`, but the agent is no longer a bound id — it is `IN`
/// the candidate subquery, so a viewer with N candidates costs the same one statement per
/// arm as a viewer with one. Each row carries its OWN `agent_id` so the caller can group
/// arms back onto the agent they describe. Binds, in order: the arm tag, then the viewer
/// id twice (creator column, then relation candidate subquery).
const SELECT_CANDIDATE_AUDIENCE_ARM: &str = concat!(
    "SELECT rel.agent_id AS agent_id, rel.user_id AS user_id FROM (",
    agent_viewer_relation!(),
    ") rel WHERE rel.arm = ? AND rel.agent_id IN (SELECT a.id FROM agents a WHERE ",
    viewer_candidate_predicate!(),
    ")"
);

/// Which set-shaped arm of the relation a read is asking for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ViewArm {
    DirectPair,
    Workspace,
}

impl ViewArm {
    /// The one spelling of this arm in the process: the relation stamps it and a
    /// read binds it from here.
    const fn tag(self) -> &'static str {
        match self {
            Self::DirectPair => direct_pair_arm_tag!(),
            Self::Workspace => workspace_arm_tag!(),
        }
    }

    /// The stored column this arm's ids come from — what a decode failure names, so
    /// the report points at the table a corrupt id actually sits in.
    const fn source_column(self) -> &'static str {
        match self {
            Self::DirectPair => "conversations.direct_pair human endpoint",
            Self::Workspace => "workspace_users.user_id",
        }
    }
}

/// The two set-shaped arms of one agent's audience, still separate.
///
/// The fields are named after the arms because the constructor they feed names
/// them: an adapter that merged them here could pass one arm twice and still
/// compile.
#[derive(Default)]
pub(crate) struct AgentAudienceArms {
    pub(crate) direct_pair_human_endpoints: Vec<UserId>,
    pub(crate) shared_workspace_members: Vec<UserId>,
}

/// Why an ACL read could not produce arms. Each consuming adapter maps this into
/// its own vocabulary; it never reaches a boundary under this name.
#[derive(Debug)]
pub(crate) enum AgentViewAclError {
    Sql(sqlx::Error),
    MalformedIdentity { column: &'static str },
}

impl From<sqlx::Error> for AgentViewAclError {
    fn from(error: sqlx::Error) -> Self {
        Self::Sql(error)
    }
}

/// Reads both arms of one agent's audience on the CALLER's connection, so they land
/// on the caller's snapshot together with the agent row they describe.
pub(crate) async fn read_agent_audience(
    connection: &mut SqliteConnection,
    agent: &AgentId,
) -> Result<AgentAudienceArms, AgentViewAclError> {
    Ok(AgentAudienceArms {
        direct_pair_human_endpoints: read_audience_arm(connection, agent, ViewArm::DirectPair)
            .await?,
        shared_workspace_members: read_audience_arm(connection, agent, ViewArm::Workspace).await?,
    })
}

async fn read_audience_arm(
    connection: &mut SqliteConnection,
    agent: &AgentId,
    arm: ViewArm,
) -> Result<Vec<UserId>, AgentViewAclError> {
    let rows = sqlx::query(SELECT_AUDIENCE_ARM)
        .bind(agent.as_str())
        .bind(arm.tag())
        .fetch_all(&mut *connection)
        .await?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        let raw: String = row.try_get("user_id")?;
        let id = UserId::parse(raw).map_err(|_| AgentViewAclError::MalformedIdentity {
            column: arm.source_column(),
        })?;
        ids.push(id);
    }
    Ok(ids)
}

/// The bounded, set-shaped audience read for the LIST face: both arms of the audience
/// for the viewer's ENTIRE candidate set, grouped by the agent each arm describes.
///
/// This is what keeps the list adapter's statement count constant. Rather than reading
/// one agent's audience per candidate (`1 + 2N`), it reads each arm ONCE across the whole
/// candidate set — a fixed three statements total with the candidate-row query. Grouping
/// keys every user under the row's OWN `agent_id`, so an audience cannot be spliced onto
/// the wrong agent; a candidate the map never mentions has an EMPTY audience (a
/// creator-only agent nobody else can see), which the caller supplies as the default.
pub(crate) async fn read_candidate_audiences(
    connection: &mut SqliteConnection,
    viewer: &UserId,
) -> Result<HashMap<String, AgentAudienceArms>, AgentViewAclError> {
    let mut audiences: HashMap<String, AgentAudienceArms> = HashMap::new();
    for (agent_id, user_id) in read_candidate_arm(connection, viewer, ViewArm::DirectPair).await? {
        audiences
            .entry(agent_id)
            .or_default()
            .direct_pair_human_endpoints
            .push(user_id);
    }
    for (agent_id, user_id) in read_candidate_arm(connection, viewer, ViewArm::Workspace).await? {
        audiences
            .entry(agent_id)
            .or_default()
            .shared_workspace_members
            .push(user_id);
    }
    Ok(audiences)
}

async fn read_candidate_arm(
    connection: &mut SqliteConnection,
    viewer: &UserId,
    arm: ViewArm,
) -> Result<Vec<(String, UserId)>, AgentViewAclError> {
    let rows = sqlx::query(SELECT_CANDIDATE_AUDIENCE_ARM)
        .bind(arm.tag())
        .bind(viewer.as_str())
        .bind(viewer.as_str())
        .fetch_all(&mut *connection)
        .await?;
    let mut pairs = Vec::with_capacity(rows.len());
    for row in rows {
        let agent_id: String = row.try_get("agent_id")?;
        let raw: String = row.try_get("user_id")?;
        let user_id = UserId::parse(raw).map_err(|_| AgentViewAclError::MalformedIdentity {
            column: arm.source_column(),
        })?;
        pairs.push((agent_id, user_id));
    }
    Ok(pairs)
}

/// The canonical active-agent audience, narrowed only by immutable agent scope.
/// Used inside HTTP snapshots and immediately before WS delivery/replay.
pub(crate) async fn may_read_agent_stream(
    connection: &mut SqliteConnection,
    viewer: &UserId,
    agent: &AgentId,
) -> Result<bool, sqlx::Error> {
    use silverhand_backend_core::identity_access::{
        agent_lifecycle::MaterializationStatus,
        capability::agent_view::{AgentViewerArms, AgentVisibility, agent_viewer_user_ids},
    };
    let row = sqlx::query("SELECT created_by_user_id, materialization_status FROM agents WHERE id=? AND session_scope='agent'")
        .bind(agent.as_str()).fetch_optional(&mut *connection).await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let creator = row
        .try_get::<Option<String>, _>("created_by_user_id")?
        .map(UserId::parse)
        .transpose()
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let status = MaterializationStatus::parse(&row.try_get::<String, _>("materialization_status")?)
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let audience = read_agent_audience(connection, agent)
        .await
        .map_err(|e| match e {
            AgentViewAclError::Sql(e) => e,
            AgentViewAclError::MalformedIdentity { column } => {
                sqlx::Error::Protocol(format!("invalid agent audience {column}"))
            }
        })?;
    Ok(agent_viewer_user_ids(Some(&AgentVisibility {
        viewers: AgentViewerArms::new(
            creator,
            audience.direct_pair_human_endpoints,
            audience.shared_workspace_members,
        ),
        materialization_status: status,
    }))
    .contains(viewer))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path};

    use tempfile::TempDir;

    use super::*;

    /// One fixture in which every arm holds a user the other arm does not, so an
    /// arm-attribution slip cannot hide behind the union:
    ///
    /// * `u-pair` — direct pair with `a1` only.
    /// * `u-both` — direct pair with `a1` AND a member of the regular workspace `a1`
    ///   joined, so a row can legitimately carry both arms.
    /// * `u-regular` — regular-workspace member only.
    /// * `u-home` — user of `a1`'s agent-home workspace only.
    /// * `u-other` — reaches `a2` only, through a workspace `a1` is not in.
    /// * `u-none` — in no arm at all.
    ///
    /// `a2` also sits in a direct pair with `a1`, so the agent↔agent case has to
    /// contribute nobody for the exact-set assertions to hold.
    ///
    /// Every pair here is agent-first because the schema allows no other order — see
    /// [`the_schema_pins_the_agent_into_the_first_endpoint_slot`].
    const SEED: &[&str] = &[
        "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES \
           ('u-pair', 'u-pair', 'Pair', 1, 1), \
           ('u-both', 'u-both', 'Both', 1, 1), \
           ('u-regular', 'u-regular', 'Regular', 1, 1), \
           ('u-home', 'u-home', 'Home', 1, 1), \
           ('u-other', 'u-other', 'Other', 1, 1), \
           ('u-none', 'u-none', 'None', 1, 1)",
        "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) VALUES \
           ('h1', 'h1', 'local', 1, 'u-pair', 'app_install')",
        concat!(
            seed_agent!(
                id = "a1",
                display_name = "A1",
                host_id = "h1",
                created_by_user_id = "u-pair",
            ),
            "; ",
            seed_agent!(
                id = "a2",
                display_name = "A2",
                host_id = "h1",
                created_by_user_id = "u-other",
            ),
        ),
        // Regular workspaces: a1 joined w-regular, a2 joined w-other.
        "INSERT INTO workspaces (id, name, name_key, created_at, updated_at) VALUES \
           ('w-regular', 'Regular', 'regular', 1, 1), \
           ('w-other', 'Other', 'other', 1, 1)",
        // Agent-home workspaces, which carry owner_agent_id instead of name_key.
        "INSERT INTO workspaces (id, name, owner_agent_id, created_at, updated_at) VALUES \
           ('w-home', 'Home', 'a1', 1, 1), \
           ('w-home-2', 'Home2', 'a2', 1, 1)",
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at) VALUES \
           ('w-regular', 'a1', 1), \
           ('w-other', 'a2', 1)",
        "INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES \
           ('w-regular', 'u-regular', 1), \
           ('w-regular', 'u-both', 1), \
           ('w-home', 'u-home', 1), \
           ('w-other', 'u-other', 1)",
        concat!(
            seed_direct_pair!(
                id = "c-pair",
                endpoint_a_kind = "agent",
                endpoint_a_id = "a1",
                endpoint_b_kind = "human",
                endpoint_b_id = "u-pair",
            ),
            "; ",
            seed_direct_pair!(
                id = "c-both",
                endpoint_a_kind = "agent",
                endpoint_a_id = "a1",
                endpoint_b_kind = "human",
                endpoint_b_id = "u-both",
            ),
            "; ",
            seed_direct_pair!(
                id = "c-agents",
                endpoint_a_kind = "agent",
                endpoint_a_id = "a1",
                endpoint_b_kind = "agent",
                endpoint_b_id = "a2",
            ),
        ),
    ];

    async fn seeded(dir: &TempDir) -> SqliteConnection {
        let path: &Path = &dir.path().join("acl.sqlite3");
        let mut connection = crate::current_schema::create_current_schema(path).await;
        for statement in SEED {
            sqlx::query(*statement)
                .execute(&mut connection)
                .await
                .expect("seed statement");
        }
        connection
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn set(ids: Vec<UserId>) -> BTreeSet<String> {
        ids.into_iter().map(|id| id.as_str().to_owned()).collect()
    }

    fn expected(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    /// Each field of the audience is supplied by its OWN arm. A misrouted field
    /// would still type-check and still union correctly downstream, so this is
    /// checked where the arms are still apart.
    #[tokio::test]
    async fn the_audience_supplies_each_field_from_its_own_arm() {
        let dir = TempDir::new().expect("temp dir");
        let mut db = seeded(&dir).await;

        let audience = read_agent_audience(&mut db, &agent("a1"))
            .await
            .expect("audience");

        assert_eq!(
            set(audience.direct_pair_human_endpoints),
            expected(&["u-pair", "u-both"])
        );
        assert_eq!(
            set(audience.shared_workspace_members),
            expected(&["u-regular", "u-both", "u-home"])
        );
    }

    /// The relation answers for the agent it was ASKED about. `a2` reaches its own
    /// workspace users and nobody through `a1`'s pairs, which a relation that
    /// dropped the `agent_id` binding could not reproduce.
    #[tokio::test]
    async fn the_relation_answers_for_the_requested_agent() {
        let dir = TempDir::new().expect("temp dir");
        let mut db = seeded(&dir).await;

        let audience = read_agent_audience(&mut db, &agent("a2"))
            .await
            .expect("audience");

        assert_eq!(set(audience.direct_pair_human_endpoints), expected(&[]));
        assert_eq!(
            set(audience.shared_workspace_members),
            expected(&["u-other"])
        );
    }

    /// The LIST direction of the same relation: binding a `user_id` yields exactly
    /// the agents that user may see, gathered across ALL arms with no partition.
    /// Read for every fixture user, this is the transpose of the audience reads
    /// above — the same membership rule seen from the viewer end. The production
    /// candidate reader lands with the `GET /api/agents` route that consumes it.
    #[tokio::test]
    async fn the_viewer_direction_yields_the_agents_a_user_may_see() {
        let dir = TempDir::new().expect("temp dir");
        let mut db = seeded(&dir).await;

        for (user_id, expected_agents) in [
            ("u-pair", &["a1"][..]),
            ("u-both", &["a1"][..]),
            ("u-regular", &["a1"][..]),
            ("u-home", &["a1"][..]),
            ("u-other", &["a2"][..]),
            ("u-none", &[][..]),
        ] {
            let rows = sqlx::query(select_viewer_candidate_ids!())
                .bind(user_id)
                .fetch_all(&mut db)
                .await
                .expect("viewer candidates");
            let seen: BTreeSet<String> = rows
                .iter()
                .map(|row| row.try_get::<String, _>("agent_id").expect("agent_id"))
                .collect();
            assert_eq!(seen, expected(expected_agents), "candidates for {user_id}");
        }
    }

    /// The current schema pins a human↔agent pair into ONE endpoint order — agent
    /// first — through a canonical-order CHECK. So the human-first half of the
    /// direct-pair arm is unreachable here. It is kept in the relation verbatim to
    /// match the TypeScript port's two-order text rather than because a row can
    /// exercise it; this test records why the fixture seeds only agent-first pairs,
    /// so a future reader does not "fix" the fixture into a constraint violation.
    #[tokio::test]
    async fn the_schema_pins_the_agent_into_the_first_endpoint_slot() {
        let dir = TempDir::new().expect("temp dir");
        let mut db = seeded(&dir).await;

        let human_first = sqlx::query(seed_direct_pair!(
            id = "c-human-first",
            endpoint_a_kind = "human",
            endpoint_a_id = "u-none",
            endpoint_b_kind = "agent",
            endpoint_b_id = "a1",
        ))
        .execute(&mut db)
        .await;

        assert!(
            human_first.is_err(),
            "the schema must reject a human-first direct_pair row"
        );
    }
}
