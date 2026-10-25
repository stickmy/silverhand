//! The agent message-search read as ONE authorized connection-bound snapshot.
//!
//! Scope resolution (the #54 conversation_id provenance matrix, workspace
//! cross-reach, `read_messages` cap, and participation), continuous streaming
//! fold-evaluation (ordered `created_at DESC, seq DESC`, NO fixed scan cap and NO
//! LIKE pre-filter, since the pinned portable full fold is not a superset of SQLite
//! ASCII LIKE, so candidate selection must share the SAME fold as the excerpt),
//! in-transaction author resolution, and per-hit placement coordinates, all on one
//! `BEGIN DEFERRED` tx.

use silverhand_backend_core::{
    conversation_messaging::messaging::FromKind,
    conversation_messaging::messaging::collaboration::agent_search::{
        AgentSearchHit, AgentSearchQuery, AgentSearchRead, AgentSearchResult, AgentSearchScope,
        ResolvedSearchScope, SearchRefusal, SearchScopeSelector, search_excerpt_for,
    },
    conversation_messaging::messaging::collaboration::message_target_coordinates::{
        ConversationPlacementKind, ConversationRosterPolicy, MessageTargetCoordinates,
    },
    foundation::identity::{AgentId, ConversationId, WorkspaceId},
    foundation::name_canonicalization::fold_case_only,
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;
use crate::conversation_messaging::messaging::author::{AuthorEvidence, resolve_authors};

/// The agent search read authority — a shared-pool clone of the ONE store.
#[derive(Clone)]
pub struct SqliteAgentSearch {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn agent_search(&self) -> SqliteAgentSearch {
        SqliteAgentSearch {
            pool: self.pool().clone(),
        }
    }
}

impl SqliteAgentSearch {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }
}

#[derive(Debug, Error)]
pub enum AgentSearchStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored message from_kind is outside the closed set")]
    UnknownFromKind,
    #[error("stored row column {0} is malformed")]
    CorruptRow(&'static str),
}

impl From<AgentSearchStoreError> for PersistenceFailure {
    fn from(value: AgentSearchStoreError) -> Self {
        Self::from_source(value)
    }
}

#[cfg(test)]
#[path = "../../../../tests/support/search_test_hooks.rs"]
mod test_hooks;
#[cfg(test)]
use test_hooks::{SEARCH_ROW_ENTERED, SEARCH_TIMING};

impl AgentSearchRead for SqliteAgentSearch {
    async fn search(
        &self,
        agent: &AgentId,
        source: Option<&ConversationId>,
        selector: &SearchScopeSelector,
        query: &AgentSearchQuery,
    ) -> Result<Result<AgentSearchResult, SearchRefusal>, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(AgentSearchStoreError::from)?;
        #[cfg(test)]
        let transaction_started = std::time::Instant::now();

        // Resolve the scope + authorize, all in-snapshot.
        let scope = match resolve_scope(&mut tx, agent, source, selector).await? {
            Ok(scope) => scope,
            Err(refusal) => {
                tx.commit().await.map_err(AgentSearchStoreError::from)?;
                return Ok(Err(refusal));
            }
        };

        // Continuous fold evaluation until `limit` hits or scope exhausted.
        let hits = collect_hits(&mut tx, agent, &scope, query).await?;

        // Resolve authors for the collected hits IN this tx.
        let evidence: Vec<AuthorEvidence> = hits.iter().map(|h| h.evidence.clone()).collect();
        let authors = resolve_authors(&mut tx, &evidence).await?;
        // Excerpt folding and DTO allocation only consume the collected facts.
        tx.commit().await.map_err(AgentSearchStoreError::from)?;
        #[cfg(test)]
        let _ = SEARCH_TIMING.try_with(|elapsed| elapsed.set(transaction_started.elapsed()));

        let mut projected = Vec::with_capacity(hits.len());
        for (hit, author) in hits.into_iter().zip(authors) {
            projected.push(AgentSearchHit {
                message_id: hit.id,
                conversation_id: hit.conversation_id,
                seq: hit.seq,
                created_at: hit.created_at,
                author,
                excerpt: search_excerpt_for(&hit.content, &query.text),
                coordinates: hit.coordinates,
            });
        }

        Ok(Ok(AgentSearchResult {
            scope: scope.echo,
            hits: projected,
        }))
    }
}

/// A resolved, authorized search scope: the SQL scope predicate coordinates + the
/// wire echo.
struct AuthorizedScope {
    /// `Some(conversation)` = single-conversation scope; `None` = workspace scope.
    conversation: Option<ConversationId>,
    /// Set for a workspace scope.
    workspace: Option<WorkspaceId>,
    echo: ResolvedSearchScope,
}

/// One collected candidate hit before author resolution.
struct RawHit {
    id: String,
    conversation_id: String,
    seq: i64,
    created_at: i64,
    content: String,
    evidence: AuthorEvidence,
    coordinates: MessageTargetCoordinates,
}

/// Resolve + authorize the scope in-snapshot.
async fn resolve_scope(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    source: Option<&ConversationId>,
    selector: &SearchScopeSelector,
) -> Result<Result<AuthorizedScope, SearchRefusal>, PersistenceFailure> {
    match selector {
        SearchScopeSelector::Conversation(target) => {
            // Provenance matrix: only an EXISTING target runs the target cap gate;
            // an unknown id falls through to the source/no-source arm (#54).
            match load_conversation_coords(tx, target).await? {
                Some(coords) => {
                    // Target exists → TARGET `read_messages` capability FIRST, then
                    // participation: the reference checks the target
                    // workspace capability before the builder-side participation, so a
                    // known target with NO cap is `capability_denied` even for a
                    // non-participant (never `not_conversation_member`).
                    if let Some(workspace) = &coords.workspace_id
                        && !may_read_messages(tx, agent, workspace).await?
                    {
                        return Ok(Err(SearchRefusal::CapabilityDenied));
                    }
                    if !agent_participates(tx, &coords, agent).await? {
                        return Ok(Err(SearchRefusal::NotConversationMember));
                    }
                    Ok(Ok(AuthorizedScope {
                        conversation: Some(target.clone()),
                        workspace: None,
                        echo: ResolvedSearchScope {
                            scope: AgentSearchScope::Conversation,
                            echo_workspace: None,
                            echo_conversation: Some(target.as_str().to_owned()),
                        },
                    }))
                }
                None => {
                    // Unknown target: fall to the source-workspace cap gate (#54).
                    match source {
                        Some(src) => match load_conversation_coords(tx, src).await? {
                            Some(src_coords) => {
                                if let Some(workspace) = &src_coords.workspace_id
                                    && !may_read_messages(tx, agent, workspace).await?
                                {
                                    return Ok(Err(SearchRefusal::CapabilityDenied));
                                }
                                Ok(Err(SearchRefusal::ConversationNotFound))
                            }
                            None => Ok(Err(SearchRefusal::ConversationNotFound)),
                        },
                        None => Ok(Err(SearchRefusal::ScopeRequired)),
                    }
                }
            }
        }
        SearchScopeSelector::Workspace(_) | SearchScopeSelector::CurrentWorkspace => {
            let source_workspace = match source {
                Some(src) => match load_conversation_coords(tx, src).await? {
                    Some(coords) => coords.workspace_id,
                    None => return Ok(Err(SearchRefusal::SourceNotFound)),
                },
                None => None,
            };
            let target = match selector {
                SearchScopeSelector::Workspace(id) => id.clone(),
                _ => match source_workspace.clone() {
                    Some(id) => id,
                    None => return Ok(Err(SearchRefusal::WorkspaceScopeRequired)),
                },
            };
            // Cross-workspace reach when the target differs from the source's ws.
            if source_workspace.as_ref() != Some(&target)
                && !agent_reaches_workspace(tx, agent, &target).await?
            {
                return Ok(Err(SearchRefusal::WorkspaceForbidden));
            }
            if !may_read_messages(tx, agent, &target).await? {
                return Ok(Err(SearchRefusal::CapabilityDenied));
            }
            Ok(Ok(AuthorizedScope {
                conversation: None,
                workspace: Some(target.clone()),
                echo: ResolvedSearchScope {
                    scope: AgentSearchScope::Workspace,
                    echo_workspace: Some(target.as_str().to_owned()),
                    echo_conversation: None,
                },
            }))
        }
        SearchScopeSelector::Current => match source {
            Some(src) => {
                // Participation is the gate for the calling conversation.
                let Some(coords) = load_conversation_coords(tx, src).await? else {
                    return Ok(Err(SearchRefusal::SourceNotFound));
                };
                if !agent_participates(tx, &coords, agent).await? {
                    return Ok(Err(SearchRefusal::NotConversationMember));
                }
                if let Some(workspace) = &coords.workspace_id
                    && !may_read_messages(tx, agent, workspace).await?
                {
                    return Ok(Err(SearchRefusal::CapabilityDenied));
                }
                Ok(Ok(AuthorizedScope {
                    conversation: Some(src.clone()),
                    workspace: None,
                    echo: ResolvedSearchScope {
                        scope: AgentSearchScope::Current,
                        echo_workspace: None,
                        echo_conversation: Some(src.as_str().to_owned()),
                    },
                }))
            }
            None => Ok(Err(SearchRefusal::ScopeRequired)),
        },
    }
}

/// Stream one ordered authorized query until enough fold matches or exhaustion.
/// No scan cap or LIKE prefilter: full Unicode folding defines inclusion. The
/// SQLx row stream bounds adapter buffering; unlike repeated keyset queries it
/// does not re-plan/re-sort the remaining scope every 256 non-matches.
async fn collect_hits(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    scope: &AuthorizedScope,
    query: &AgentSearchQuery,
) -> Result<Vec<RawHit>, PersistenceFailure> {
    use futures_util::TryStreamExt;
    let folded_needle = fold_case_only(&query.text);
    let mut hits = Vec::new();
    let (statement, scope_id) = match (&scope.conversation, query.include_system) {
        (Some(conversation), true) => (CONV_ALL, conversation.as_str()),
        (Some(conversation), false) => (CONV_NOSYS, conversation.as_str()),
        (None, true) => (
            WS_ALL,
            scope.workspace.as_ref().expect("workspace scope").as_str(),
        ),
        (None, false) => (
            WS_NOSYS,
            scope.workspace.as_ref().expect("workspace scope").as_str(),
        ),
    };
    let mut rows = sqlx::query(statement)
        .bind(agent.as_str())
        .bind(scope_id)
        .fetch(&mut **tx);
    while let Some(row) = rows.try_next().await.map_err(AgentSearchStoreError::from)? {
        #[cfg(test)]
        if let Ok(entered) = SEARCH_ROW_ENTERED.try_with(Clone::clone) {
            entered.notify_one();
            std::future::pending::<()>().await;
        }
        let raw = decode_raw_hit(&row)?;
        if fold_case_only(&raw.content).contains(&folded_needle) {
            hits.push(raw);
            if hits.len() as i64 >= query.limit {
                break;
            }
        }
    }
    Ok(hits)
}

macro_rules! candidate_sql {
    ($scopepred:literal, $system:literal) => {
        concat!(
            "SELECT m.id AS id, m.conversation_id AS conversation_id, m.content AS content, \
             m.seq AS seq, m.created_at AS created_at, m.from_kind AS from_kind, \
             m.from_agent_id AS from_agent_id, m.from_user_id AS from_user_id, \
             m.author_id AS author_id, m.author_name AS author_name, \
             m.author_display_name AS author_display_name, \
             conv.placement_kind AS placement_kind, conv.roster_policy AS roster_policy, \
             conv.workspace_id AS workspace_id, conv.title AS ctitle \
             FROM messages m JOIN conversations conv ON conv.id = m.conversation_id \
             WHERE (\
               (conv.placement_kind = 'workspace' AND EXISTS (\
                  SELECT 1 FROM conversation_agents ca_p \
                   WHERE ca_p.conversation_id = conv.id AND ca_p.agent_id = ?1)) \
               OR (conv.placement_kind = 'direct_pair' AND (\
                    (conv.endpoint_a_kind = 'agent' AND conv.endpoint_a_id = ?1) \
                 OR (conv.endpoint_b_kind = 'agent' AND conv.endpoint_b_id = ?1)))) \
             AND ",
            $scopepred,
            " ",
            $system,
            " ORDER BY m.created_at DESC, m.seq DESC, m.id DESC"
        )
    };
}

// seq is only conversation-local: id remains the global tie breaker.
const CONV_ALL: &str = candidate_sql!("conv.id = ?2", "");
const CONV_NOSYS: &str = candidate_sql!("conv.id = ?2", "AND m.from_kind <> 'system'");
const WS_ALL: &str = candidate_sql!("conv.workspace_id = ?2", "");
const WS_NOSYS: &str = candidate_sql!("conv.workspace_id = ?2", "AND m.from_kind <> 'system'");

/// JS-safe integer gate at store→projection: a stored numeric JS can't
/// represent exactly is a read error, not a truncated wire number.
fn search_js_safe(value: i64, column: &'static str) -> Result<i64, PersistenceFailure> {
    silverhand_backend_core::foundation::wire_number::JsSafeInteger::new(value)
        .map(|v| v.get())
        .map_err(|_| PersistenceFailure::from_source(AgentSearchStoreError::CorruptRow(column)))
}

fn decode_raw_hit(row: &sqlx::sqlite::SqliteRow) -> Result<RawHit, PersistenceFailure> {
    let conversation_id: String = row
        .try_get("conversation_id")
        .map_err(AgentSearchStoreError::from)?;
    let placement_raw: String = row
        .try_get("placement_kind")
        .map_err(AgentSearchStoreError::from)?;
    let placement_kind = ConversationPlacementKind::parse(&placement_raw).ok_or(
        AgentSearchStoreError::CorruptRow("conversations.placement_kind"),
    )?;
    let roster_raw: String = row
        .try_get("roster_policy")
        .map_err(AgentSearchStoreError::from)?;
    let roster_policy = ConversationRosterPolicy::parse(&roster_raw).ok_or(
        AgentSearchStoreError::CorruptRow("conversations.roster_policy"),
    )?;
    let workspace_raw: Option<String> = row
        .try_get("workspace_id")
        .map_err(AgentSearchStoreError::from)?;
    let workspace_id = workspace_raw
        .map(WorkspaceId::parse)
        .transpose()
        .map_err(|_| AgentSearchStoreError::CorruptRow("conversations.workspace_id"))?;
    let conversation = ConversationId::parse(conversation_id.clone())
        .map_err(|_| AgentSearchStoreError::CorruptRow("messages.conversation_id"))?;
    Ok(RawHit {
        id: row.try_get("id").map_err(AgentSearchStoreError::from)?,
        conversation_id,
        seq: search_js_safe(
            row.try_get("seq").map_err(AgentSearchStoreError::from)?,
            "messages.seq",
        )?,
        created_at: search_js_safe(
            row.try_get("created_at")
                .map_err(AgentSearchStoreError::from)?,
            "messages.created_at",
        )?,
        content: row
            .try_get("content")
            .map_err(AgentSearchStoreError::from)?,
        evidence: decode_evidence(row)?,
        coordinates: MessageTargetCoordinates {
            conversation: conversation.clone(),
            placement_kind,
            roster_policy,
            channel_id: conversation,
            workspace_id,
            title: row.try_get("ctitle").map_err(AgentSearchStoreError::from)?,
        },
    })
}

/// The conversation's placement coordinates, or `None` if it does not exist.
async fn load_conversation_coords(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
) -> Result<Option<MessageTargetCoordinates>, PersistenceFailure> {
    let Some(row) = sqlx::query(
        "SELECT placement_kind, roster_policy, workspace_id, title FROM conversations WHERE id = ?",
    )
    .bind(conversation.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(AgentSearchStoreError::from)?
    else {
        return Ok(None);
    };
    let placement_raw: String = row
        .try_get("placement_kind")
        .map_err(AgentSearchStoreError::from)?;
    let placement_kind = ConversationPlacementKind::parse(&placement_raw).ok_or(
        AgentSearchStoreError::CorruptRow("conversations.placement_kind"),
    )?;
    let roster_raw: String = row
        .try_get("roster_policy")
        .map_err(AgentSearchStoreError::from)?;
    let roster_policy = ConversationRosterPolicy::parse(&roster_raw).ok_or(
        AgentSearchStoreError::CorruptRow("conversations.roster_policy"),
    )?;
    let workspace_raw: Option<String> = row
        .try_get("workspace_id")
        .map_err(AgentSearchStoreError::from)?;
    let workspace_id = workspace_raw
        .map(WorkspaceId::parse)
        .transpose()
        .map_err(|_| AgentSearchStoreError::CorruptRow("conversations.workspace_id"))?;
    let title: String = row.try_get("title").map_err(AgentSearchStoreError::from)?;
    Ok(Some(MessageTargetCoordinates {
        conversation: conversation.clone(),
        placement_kind,
        roster_policy,
        channel_id: conversation.clone(),
        workspace_id,
        title,
    }))
}

/// Agent participation in a conversation (workspace → conversation_agents; direct_pair
/// → non-deleted agent endpoint).
async fn agent_participates(
    tx: &mut Transaction<'_, Sqlite>,
    coords: &MessageTargetCoordinates,
    agent: &AgentId,
) -> Result<bool, PersistenceFailure> {
    match coords.placement_kind {
        ConversationPlacementKind::Workspace => {
            let hit: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM conversation_agents WHERE conversation_id = ? AND agent_id = ?",
            )
            .bind(coords.conversation.as_str())
            .bind(agent.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(AgentSearchStoreError::from)?;
            Ok(hit.is_some())
        }
        ConversationPlacementKind::DirectPair => {
            let hit: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM conversations WHERE id = ? AND placement_kind = 'direct_pair' \
                 AND ((endpoint_a_kind = 'agent' AND endpoint_a_id = ? AND endpoint_a_deleted_at IS NULL) \
                   OR (endpoint_b_kind = 'agent' AND endpoint_b_id = ? AND endpoint_b_deleted_at IS NULL))",
            )
            .bind(coords.conversation.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(AgentSearchStoreError::from)?;
            Ok(hit.is_some())
        }
    }
}

/// `read_messages` on a workspace (owner arm ∪ explicit grant).
async fn may_read_messages(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    workspace: &WorkspaceId,
) -> Result<bool, PersistenceFailure> {
    let granted: Option<i64> = sqlx::query_scalar(
        "SELECT CASE WHEN w.owner_agent_id = ? OR EXISTS (SELECT 1 FROM \
         workspace_agent_capabilities c WHERE c.workspace_id = w.id AND c.agent_id = ? \
         AND c.capability = 'read_messages') THEN 1 ELSE 0 END FROM workspaces w WHERE w.id = ?",
    )
    .bind(agent.as_str())
    .bind(agent.as_str())
    .bind(workspace.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(AgentSearchStoreError::from)?;
    Ok(granted.is_some_and(|allowed| allowed != 0))
}

/// The caller's read horizon on a workspace: regular membership ∪ owned agent-home.
async fn agent_reaches_workspace(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    workspace: &WorkspaceId,
) -> Result<bool, PersistenceFailure> {
    let reaches: Option<i64> = sqlx::query_scalar(
        "SELECT CASE WHEN w.owner_agent_id = ? OR (w.owner_agent_id IS NULL AND EXISTS (\
         SELECT 1 FROM workspace_members wm WHERE wm.workspace_id = w.id AND wm.agent_id = ?)) \
         THEN 1 ELSE 0 END FROM workspaces w WHERE w.id = ?",
    )
    .bind(agent.as_str())
    .bind(agent.as_str())
    .bind(workspace.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(AgentSearchStoreError::from)?;
    Ok(reaches.is_some_and(|allowed| allowed != 0))
}

fn decode_evidence(row: &sqlx::sqlite::SqliteRow) -> Result<AuthorEvidence, AgentSearchStoreError> {
    let from_kind_raw: String = row.try_get("from_kind")?;
    let from_kind = match from_kind_raw.as_str() {
        "user" => FromKind::User,
        "agent" => FromKind::Agent,
        "system" => FromKind::System,
        _ => return Err(AgentSearchStoreError::UnknownFromKind),
    };
    Ok(AuthorEvidence {
        from_kind,
        from_agent_id: row.try_get("from_agent_id")?,
        from_user_id: row.try_get("from_user_id")?,
        author_id: row.try_get("author_id")?,
        author_name: row.try_get("author_name")?,
        author_display_name: row.try_get("author_display_name")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::agent_search::AgentSearchScope;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str = include_str!(
        "../../../../../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    );

    // ws with channel `chan` (agent-a member+cap, agent-c member no-cap, agent-x
    // nonmember); messages: m1 "straße rules" (fold hit for SS), m2 "plain".
    const SEED: &str = "\
INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('u1','u','U',1,1); \
INSERT INTO hosts (id,name,kind,created_at) VALUES ('h1','H','local',1); \
INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES ('agent-a','alice','Alice','claude','h1','u1',1,1,'conversation'); \
INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES ('agent-c','dave','Dave','codex','h1','u1',1,1,'conversation'); \
INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES ('agent-x','carol','Carol','codex','h1','u1',1,1,'conversation'); \
INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES ('agent-y','erin','Erin','codex','h1','u1',1,1,'conversation'); \
INSERT INTO workspaces (id,name,name_key,owner_agent_id,created_at,updated_at) VALUES ('ws','Team','team',NULL,1,1); \
INSERT INTO workspace_members (workspace_id,agent_id,joined_at) VALUES ('ws','agent-a',1); \
INSERT INTO workspace_members (workspace_id,agent_id,joined_at) VALUES ('ws','agent-c',1); \
INSERT INTO workspace_members (workspace_id,agent_id,joined_at) VALUES ('ws','agent-y',1); \
INSERT INTO workspace_agent_capabilities (workspace_id,agent_id,capability) VALUES ('ws','agent-a','read_messages'); \
INSERT INTO workspace_agent_capabilities (workspace_id,agent_id,capability) VALUES ('ws','agent-y','read_messages'); \
INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES ('chan','ws','workspace','open','Chan','chan','idle','private',1,1); \
INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('chan','agent-a',1); \
INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('chan','agent-c',1); \
INSERT INTO messages (id,conversation_id,from_kind,from_agent_id,content,seq,created_at,author_id,author_name,author_display_name) VALUES ('m1','chan','agent','agent-a','the straße rules',1,10,'agent-a','alice','Alice'); \
INSERT INTO messages (id,conversation_id,from_kind,from_agent_id,content,seq,created_at,author_id,author_name,author_display_name) VALUES ('m2','chan','agent','agent-a','plain text',2,20,'agent-a','alice','Alice');";

    async fn seeded() -> (tempfile::TempDir, SqliteAgentSearch) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sqlite");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED)).await.unwrap();
        db.close().await.unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        (dir, SqliteAgentSearch::from_pool(pool))
    }
    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).unwrap()
    }
    fn conv(id: &str) -> ConversationId {
        ConversationId::parse(id).unwrap()
    }
    fn query(text: &str) -> AgentSearchQuery {
        AgentSearchQuery {
            text: text.to_owned(),
            include_system: true,
            limit: 50,
        }
    }

    #[tokio::test]
    async fn streamed_search_releases_single_connection_on_limit_error_and_cancel() {
        use std::{sync::Arc, time::Duration};
        let (dir, original) = seeded().await;
        original.pool.close().await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(dir.path().join("s.sqlite")))
            .await
            .unwrap();
        let read = SqliteAgentSearch::from_pool(pool.clone());
        let mut q = query("text");
        q.limit = 1;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            read.search(
                &agent("agent-a"),
                None,
                &SearchScopeSelector::Conversation(conv("chan")),
                &q,
            ),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(result.hits.len(), 1);
        assert!(
            result.hits[0].author.is_some(),
            "author lookup reuses the stream's connection"
        );
        pool.begin_with("BEGIN IMMEDIATE")
            .await
            .unwrap()
            .rollback()
            .await
            .unwrap();

        let entered = Arc::new(tokio::sync::Notify::new());
        let cloned = read.clone();
        let task = tokio::spawn(SEARCH_ROW_ENTERED.scope(entered.clone(), async move {
            cloned
                .search(
                    &agent("agent-a"),
                    None,
                    &SearchScopeSelector::Conversation(conv("chan")),
                    &query("text"),
                )
                .await
        }));
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), pool.begin_with("BEGIN IMMEDIATE"))
            .await
            .unwrap()
            .unwrap()
            .rollback()
            .await
            .unwrap();

        sqlx::query("UPDATE messages SET seq=9007199254740993 WHERE id='m2'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            read.search(
                &agent("agent-a"),
                None,
                &SearchScopeSelector::Conversation(conv("chan")),
                &query("text")
            )
            .await
            .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), pool.begin_with("BEGIN IMMEDIATE"))
            .await
            .unwrap()
            .unwrap()
            .rollback()
            .await
            .unwrap();
        pool.close().await;
    }

    /// Manual bounded scale evidence, not a noisy wall-clock CI threshold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "scale measurement: run explicitly with --ignored --nocapture"]
    async fn search_scale_request_transaction_and_writer_wait() {
        use std::{
            cell::Cell,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            time::{Duration, Instant},
        };
        for size in [1_000_i64, 10_000] {
            let (_dir, read) = seeded().await;
            sqlx::query("PRAGMA journal_mode=WAL")
                .execute(&read.pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM messages")
                .execute(&read.pool)
                .await
                .unwrap();
            let content = "x".repeat(1024);
            sqlx::query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < ?) INSERT INTO messages(id,conversation_id,from_kind,content,seq,created_at) SELECT 'scale-' || i,'chan','system',CASE WHEN i=1 THEN ? || ' straße' ELSE ? END,i,i FROM n")
                .bind(size).bind(&content).bind(&content).execute(&read.pool).await.unwrap();
            for (needle, expected) in [("not-present-needle", 0), ("SS", 1)] {
                let stop = Arc::new(AtomicBool::new(false));
                let writer_stop = stop.clone();
                let writer_pool = read.pool.clone();
                let writer = tokio::spawn(async move {
                    let mut waits = Vec::new();
                    while !writer_stop.load(Ordering::Relaxed) {
                        let start = Instant::now();
                        let mut tx = writer_pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
                        waits.push(start.elapsed().as_micros());
                        sqlx::query(
                            "UPDATE conversations SET updated_at=updated_at+1 WHERE id='chan'",
                        )
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                        tx.commit().await.unwrap();
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    waits
                });
                let mut requests = Vec::new();
                let mut transactions = Vec::new();
                for _ in 0..3 {
                    SEARCH_TIMING
                        .scope(Cell::new(Duration::ZERO), async {
                            let start = Instant::now();
                            let result = read
                                .search(
                                    &agent("agent-a"),
                                    Some(&conv("chan")),
                                    &SearchScopeSelector::Conversation(conv("chan")),
                                    &query(needle),
                                )
                                .await
                                .unwrap()
                                .unwrap();
                            requests.push(start.elapsed().as_micros());
                            transactions
                                .push(SEARCH_TIMING.with(|elapsed| elapsed.get().as_micros()));
                            assert_eq!(result.hits.len(), expected);
                            if expected == 1 {
                                assert_eq!(result.hits[0].message_id, "scale-1");
                            }
                        })
                        .await;
                }
                stop.store(true, Ordering::Relaxed);
                let mut waits = writer.await.unwrap();
                waits.sort_unstable();
                assert!(!waits.is_empty());
                println!(
                    "search_scale rows={size} bytes_per_row=1024 needle={needle} hits={expected} requests_us={requests:?} transactions_us={transactions:?} concurrent_writers=1 writer_samples={} writer_wait_p50_us={} writer_wait_p95_us={}",
                    waits.len(),
                    waits[waits.len() / 2],
                    waits[(waits.len() - 1) * 95 / 100]
                );
            }
            read.pool.close().await;
        }
    }

    #[tokio::test]
    async fn fold_match_finds_a_non_like_hit_and_carries_per_hit_coords() {
        let (_d, read) = seeded().await;
        // "SS" folds to match "ß" — an ASCII LIKE '%SS%' would NOT return m1, proving
        // candidate selection shares the portable fold (#37/#50).
        let result = read
            .search(
                &agent("agent-a"),
                Some(&conv("chan")),
                &SearchScopeSelector::Conversation(conv("chan")),
                &query("SS"),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].message_id, "m1");
        // Per-hit placement coordinates.
        let c = &result.hits[0].coordinates;
        assert_eq!(c.placement_kind, ConversationPlacementKind::Workspace);
        assert_eq!(c.workspace_id.as_ref().unwrap().as_str(), "ws");
        assert_eq!(c.channel_id.as_str(), "chan");
        // Author resolved in-tx.
        assert!(result.hits[0].author.is_some());
        assert_eq!(result.scope.scope, AgentSearchScope::Conversation);
    }

    #[tokio::test]
    async fn conversation_selector_provenance_matrix() {
        let (_d, read) = seeded().await;
        // known target + participant + no cap (agent-c) → CapabilityDenied (#3).
        assert!(matches!(
            read.search(
                &agent("agent-c"),
                Some(&conv("chan")),
                &SearchScopeSelector::Conversation(conv("chan")),
                &query("plain")
            )
            .await
            .unwrap(),
            Err(SearchRefusal::CapabilityDenied)
        ));
        // known target + nonparticipant + no-cap (agent-x) → CapabilityDenied, NOT
        // NotConversationMember: the corrected order checks the TARGET workspace
        // read_messages capability BEFORE participation.
        assert!(matches!(
            read.search(
                &agent("agent-x"),
                Some(&conv("chan")),
                &SearchScopeSelector::Conversation(conv("chan")),
                &query("plain")
            )
            .await
            .unwrap(),
            Err(SearchRefusal::CapabilityDenied)
        ));
        // known target + CAPPED nonparticipant (agent-y: ws member + read_messages, but
        // NOT in `chan`) → NotConversationMember: participation still gates AFTER the
        // cap check passes (proves the reorder didn't drop the participation gate).
        assert!(matches!(
            read.search(
                &agent("agent-y"),
                Some(&conv("chan")),
                &SearchScopeSelector::Conversation(conv("chan")),
                &query("plain")
            )
            .await
            .unwrap(),
            Err(SearchRefusal::NotConversationMember)
        ));
        // unknown target, ConversationSlot source(chan) with cap → ConversationNotFound
        // AFTER the source-ws cap gate (#54): agent-a has cap so it reaches 404.
        assert!(matches!(
            read.search(
                &agent("agent-a"),
                Some(&conv("chan")),
                &SearchScopeSelector::Conversation(conv("nope")),
                &query("x")
            )
            .await
            .unwrap(),
            Err(SearchRefusal::ConversationNotFound)
        ));
        // unknown target, source with NO cap (agent-c) → CapabilityDenied first (#54).
        assert!(matches!(
            read.search(
                &agent("agent-c"),
                Some(&conv("chan")),
                &SearchScopeSelector::Conversation(conv("nope")),
                &query("x")
            )
            .await
            .unwrap(),
            Err(SearchRefusal::CapabilityDenied)
        ));
        // unknown target, AgentSlot (no source) → ScopeRequired (#54).
        assert!(matches!(
            read.search(
                &agent("agent-a"),
                None,
                &SearchScopeSelector::Conversation(conv("nope")),
                &query("x")
            )
            .await
            .unwrap(),
            Err(SearchRefusal::ScopeRequired)
        ));
    }

    #[tokio::test]
    async fn workspace_scope_caps_and_current_agent_slot_requires_scope() {
        let (_d, read) = seeded().await;
        // workspace scope, capped agent → hits.
        let ok = read
            .search(
                &agent("agent-a"),
                Some(&conv("chan")),
                &SearchScopeSelector::CurrentWorkspace,
                &query("plain"),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ok.scope.scope, AgentSearchScope::Workspace);
        assert_eq!(ok.hits.len(), 1);
        // AgentSlot (no source) + Current → ScopeRequired.
        assert!(matches!(
            read.search(
                &agent("agent-a"),
                None,
                &SearchScopeSelector::Current,
                &query("x")
            )
            .await
            .unwrap(),
            Err(SearchRefusal::ScopeRequired)
        ));
    }

    #[tokio::test]
    async fn revoke_read_messages_flips_the_next_workspace_search_closed() {
        let (dir, read) = seeded().await;
        assert!(
            read.search(
                &agent("agent-a"),
                Some(&conv("chan")),
                &SearchScopeSelector::Workspace(WorkspaceId::parse("ws").unwrap()),
                &query("plain")
            )
            .await
            .unwrap()
            .is_ok()
        );
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.path().join("s.sqlite"))
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id='ws' AND agent_id='agent-a'")
            .execute(&pool).await.unwrap();
        assert!(matches!(
            read.search(
                &agent("agent-a"),
                Some(&conv("chan")),
                &SearchScopeSelector::Workspace(WorkspaceId::parse("ws").unwrap()),
                &query("plain")
            )
            .await
            .unwrap(),
            Err(SearchRefusal::CapabilityDenied)
        ));
    }

    #[tokio::test]
    async fn workspace_stream_does_not_skip_a_tie_across_conversations_beyond_old_page_boundary() {
        // `seq` is unique only WITHIN a conversation, so a workspace scope can
        // hold many rows sharing the same (created_at, seq) across conversations. Seed
        // >CANDIDATE_PAGE (256) such tied NON-matching rows plus ONE match whose
        // (created_at, seq) ties the boundary row but whose id sorts AFTER it — under
        // the old (created_at, seq)-only cursor the `< pair` predicate would skip the
        // remaining tied rows (including the match). The full (created_at, seq, id)
        // keyset must still return it.
        let (dir, read) = seeded().await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.path().join("s.sqlite"))
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        // A second channel agent-a can read.
        sqlx::query("INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES ('chan2','ws','workspace','open','C2','c2','idle','private',1,1)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('chan2','agent-a',1)")
            .execute(&pool).await.unwrap();
        // chan2 gets EXACTLY 256 fillers (seq 1..=256, created_at=500), filling the
        // first DESC page precisely; NONE match. In DESC (created_at, seq, id) the last
        // row of page 1 is chan2's (500, seq=1, id=`hi-000001`). Ids `hi-*` sort HIGH.
        for seq in 1..=256i64 {
            sqlx::query(
                "INSERT INTO messages (id,conversation_id,from_kind,from_agent_id,content,seq,created_at,author_id,author_name,author_display_name) \
                 VALUES (?, 'chan2','agent','agent-a','filler', ?, 500, 'agent-a','alice','Alice')",
            )
            .bind(format!("hi-{seq:06}"))
            .bind(seq)
            .execute(&pool)
            .await
            .unwrap();
        }
        // The ONLY match is in a THIRD conversation at (500, seq=1) — the SAME
        // (created_at, seq) pair as chan2's page-1-boundary row, with a LOWER id
        // (`aa-match` < `hi-000001`). (A third conversation is needed because
        // UNIQUE(conversation_id, seq) forbids reusing seq=1 within chan2.) In the full
        // (created_at, seq, id) DESC order it is the row IMMEDIATELY after the page-1
        // boundary. An (created_at, seq)-only cursor advancing past (500,1) at the
        // boundary emits `< (500,1)` and SKIPS this tied match; the (created_at, seq,
        // id) keyset emits `< (500,1,hi-000001)` and still returns it.
        sqlx::query("INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES ('chan3','ws','workspace','open','C3','c3','idle','private',1,1)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('chan3','agent-a',1)")
            .execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO messages (id,conversation_id,from_kind,from_agent_id,content,seq,created_at,author_id,author_name,author_display_name) \
             VALUES ('aa-match','chan3','agent','agent-a','the zzqfind needle', 1, 500,'agent-a','alice','Alice')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = read
            .search(
                &agent("agent-a"),
                None,
                &SearchScopeSelector::Workspace(WorkspaceId::parse("ws").unwrap()),
                &query("zzqfind"),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result.hits.len(),
            1,
            "the sole match must survive a tie straddling the 256-row page boundary across conversations"
        );
        assert_eq!(result.hits[0].message_id, "aa-match");
    }
}
