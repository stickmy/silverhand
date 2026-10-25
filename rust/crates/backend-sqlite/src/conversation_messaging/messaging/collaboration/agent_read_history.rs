//! The agent read-history windowed read over `messages`.
//!
//! Four window modes (before/after/head/around), thread-cluster scoping (`id = root
//! OR thread_root_id = root`), an optional system filter, ordered ascending seq.
//! Each row is projected with its resolved author (shared `resolve_authors`), its
//! reaction aggregate (shared `aggregate_facts_in_snapshot`), its attachment
//! snapshots (decoded from the message metadata JSON), the snippet, and the raw
//! metadata. `has_more_before`/`_after` are probed at the window edges. The around
//! window is `floor((limit-1)/2)` before + the anchor + the rest after; an unknown
//! anchor message is the reference `404 message_not_found`.

use silverhand_backend_core::{
    conversation_messaging::messaging::FromKind,
    conversation_messaging::messaging::MessageId,
    conversation_messaging::messaging::collaboration::agent_read_history::{
        AgentReadHistoryRead, ReadHistoryAnchor, ReadHistoryMessage, ReadHistoryRefusal,
        ReadHistoryWindow, around_before_count, snippet_for,
    },
    conversation_messaging::messaging::collaboration::message_target_coordinates::{
        ConversationPlacementKind, ConversationRosterPolicy, MessageTargetCoordinates,
    },
    conversation_messaging::messaging::collaboration::reaction::Reactor,
    foundation::identity::{AgentId, ConversationId, WorkspaceId},
    foundation::name_canonicalization::is_ecmascript_trim_whitespace,
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, Sqlite, Transaction};
use std::collections::HashMap;
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;
use crate::conversation_messaging::messaging::author::{AuthorEvidence, resolve_authors};
use crate::conversation_messaging::messaging::collaboration::reaction::aggregate_facts_in_snapshot;

/// The read-history read authority — a shared-pool clone of the ONE store.
#[derive(Clone)]
pub struct SqliteAgentReadHistory {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn agent_read_history(&self) -> SqliteAgentReadHistory {
        SqliteAgentReadHistory {
            pool: self.pool().clone(),
        }
    }
}

impl SqliteAgentReadHistory {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }
}

#[derive(Debug, Error)]
pub enum AgentReadHistoryStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored message from_kind is outside the closed set")]
    UnknownFromKind,
    #[error("stored conversation workspace identity is malformed")]
    MalformedIdentity,
    #[error("stored message reaction aggregate is malformed")]
    Reaction,
    #[error("stored row column {0} is malformed")]
    CorruptRow(&'static str),
}

impl From<AgentReadHistoryStoreError> for PersistenceFailure {
    fn from(value: AgentReadHistoryStoreError) -> Self {
        Self::from_source(value)
    }
}

/// One raw window row before author/reaction resolution.
struct RawRow {
    id: String,
    conversation_id: String,
    content: String,
    metadata_json: Option<String>,
    seq: i64,
    created_at: i64,
    thread_root_id: Option<String>,
    evidence: AuthorEvidence,
}

const COLUMNS: &str = "\
    m.id AS id, m.conversation_id AS conversation_id, m.content AS content, \
    m.metadata AS metadata, m.seq AS seq, m.created_at AS created_at, \
    m.thread_root_id AS thread_root_id, m.from_kind AS from_kind, \
    m.from_agent_id AS from_agent_id, m.from_user_id AS from_user_id, \
    m.author_id AS author_id, m.author_name AS author_name, \
    m.author_display_name AS author_display_name";

impl AgentReadHistoryRead for SqliteAgentReadHistory {
    async fn read_history(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
        thread_root: Option<&str>,
        anchor: &ReadHistoryAnchor,
        include_system: bool,
        include_content: bool,
        limit: i64,
    ) -> Result<Result<ReadHistoryWindow, ReadHistoryRefusal>, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(AgentReadHistoryStoreError::from)?;

        // AUTHORIZE IN-SNAPSHOT: target existence + placement coords +
        // agent live participation + workspace `read_messages`, all on this tx BEFORE
        // any window read, so a revoke/rename/delete race cannot leak an old window.
        let coords = match authorize_read_target(&mut tx, agent, conversation).await? {
            Ok(coords) => coords,
            Err(refusal) => {
                tx.commit()
                    .await
                    .map_err(AgentReadHistoryStoreError::from)?;
                return Ok(Err(refusal));
            }
        };

        let (rows, has_more_before, has_more_after) = match anchor {
            ReadHistoryAnchor::Head => {
                // The latest `limit`, ascending. Uniform edge rule: probe from the
                // ASSEMBLED window's min/max seq (empty window → both flags false),
                // matching TS which uses `rows[0]`/`rows.at(-1)` after every mode.
                let rows = window_before(
                    &mut tx,
                    conversation,
                    thread_root,
                    include_system,
                    i64::MAX,
                    limit,
                )
                .await?;
                let (more_before, more_after) =
                    window_edges(&mut tx, conversation, thread_root, include_system, &rows).await?;
                (rows, more_before, more_after)
            }
            ReadHistoryAnchor::BeforeSeq(before) => {
                let rows = window_before(
                    &mut tx,
                    conversation,
                    thread_root,
                    include_system,
                    *before,
                    limit,
                )
                .await?;
                let (more_before, more_after) =
                    window_edges(&mut tx, conversation, thread_root, include_system, &rows).await?;
                (rows, more_before, more_after)
            }
            ReadHistoryAnchor::AfterSeq(after) => {
                let rows = window_after(
                    &mut tx,
                    conversation,
                    thread_root,
                    include_system,
                    *after,
                    limit,
                )
                .await?;
                let (more_before, more_after) =
                    window_edges(&mut tx, conversation, thread_root, include_system, &rows).await?;
                (rows, more_before, more_after)
            }
            ReadHistoryAnchor::Around(message_id) => {
                let Some(anchor_seq) =
                    anchor_seq(&mut tx, conversation, thread_root, message_id).await?
                else {
                    tx.commit()
                        .await
                        .map_err(AgentReadHistoryStoreError::from)?;
                    return Ok(Err(ReadHistoryRefusal::AnchorNotFound));
                };
                let before_budget = around_before_count(limit).before_count;
                let before_rows = window_before(
                    &mut tx,
                    conversation,
                    thread_root,
                    include_system,
                    anchor_seq,
                    before_budget,
                )
                .await?;
                let anchor_row = row_at_seq(
                    &mut tx,
                    conversation,
                    thread_root,
                    include_system,
                    anchor_seq,
                )
                .await?;
                let after_budget =
                    limit - before_rows.len() as i64 - i64::from(anchor_row.is_some());
                let after_rows = window_after(
                    &mut tx,
                    conversation,
                    thread_root,
                    include_system,
                    anchor_seq,
                    after_budget.max(0),
                )
                .await?;
                // Assemble the FULL window (before + anchor + after) FIRST, then probe
                // has_more from the window's actual min/max seq — matching TS, which
                // slices the assembled rows and probes their boundaries. Probing each
                // side's own rows is wrong when a side's budget is 0 or empty (e.g.
                // limit=1: before/after are empty, but messages exist on both sides and
                // the edges are the anchor's seq — not `None`).
                let mut rows = before_rows;
                if let Some(anchor_row) = anchor_row {
                    rows.push(anchor_row);
                }
                rows.extend(after_rows);
                let (more_before, more_after) =
                    window_edges(&mut tx, conversation, thread_root, include_system, &rows).await?;
                (rows, more_before, more_after)
            }
        };

        // Resolve authors + reactions for the projected window in ONE snapshot
        // authors in-tx; #14/#36/#36b reactions as a SINGLE bulk aggregate with
        // this agent as the viewer — not a per-row query, not hardwired false).
        let evidence: Vec<AuthorEvidence> = rows.iter().map(|row| row.evidence.clone()).collect();
        // resolve_authors takes a &mut SqliteConnection; the tx derefs to one.
        let authors = resolve_authors(&mut tx, &evidence).await?;

        let message_ids: Vec<MessageId> = rows
            .iter()
            .map(|row| MessageId::parse(row.id.clone()))
            .collect::<Result<_, _>>()
            .map_err(|_| {
                PersistenceFailure::from_source(AgentReadHistoryStoreError::CorruptRow(
                    "messages.id",
                ))
            })?;
        let reactions = reaction_aggregates_for(&mut tx, agent, &message_ids).await?;
        // All database facts are owned now. DTO/metadata work needs no connection.
        tx.commit()
            .await
            .map_err(AgentReadHistoryStoreError::from)?;

        let mut messages = Vec::with_capacity(rows.len());
        for (row, author) in rows.into_iter().zip(authors) {
            // Reactions: the aggregated array for this message, or `Null` when it has
            // none — TS emits `reactions: null` (NOT `[]`) for a message with no
            // reactions. A parse failure cannot occur here (the aggregate is
            // built in-process).
            let reactions = reactions
                .get(&row.id)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            // Fail-closed metadata decode (#14): a corrupt stored blob is a typed read
            // error, NOT a fake `null`. Metadata projects as the object or `Null` (a
            // genuinely absent object), never a route-parsed string.
            let metadata =
                decode_metadata(row.metadata_json.as_deref())?.unwrap_or(serde_json::Value::Null);
            // Attachments are the STRICT snapshot array derived from the (already
            // fail-closed) metadata — a sealed `Value`, or `Null` when none (#7).
            let attachments = attachment_snapshots(match &metadata {
                serde_json::Value::Null => None,
                other => Some(other),
            });
            messages.push(ReadHistoryMessage {
                snippet: snippet_for(&row.content),
                content: if include_content {
                    row.content.clone()
                } else {
                    String::new()
                },
                message_id: row.id,
                conversation_id: row.conversation_id,
                seq: row.seq,
                created_at: row.created_at,
                thread_root_id: row.thread_root_id,
                author,
                metadata,
                attachments,
                reactions,
            });
        }
        Ok(Ok(ReadHistoryWindow {
            messages,
            has_more_before,
            has_more_after,
            target: coords,
        }))
    }
}

/// Authorize a read-history read INSIDE the snapshot: resolve the target
/// conversation's placement coordinates (existence), the agent's live participation,
/// and the workspace `read_messages` capability (a `direct_pair` has no capability
/// axis). Returns the full [`MessageTargetCoordinates`] on success, or the typed
/// refusal — all from this one transaction.
async fn authorize_read_target(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    conversation: &ConversationId,
) -> Result<Result<MessageTargetCoordinates, ReadHistoryRefusal>, PersistenceFailure> {
    let Some(row) = sqlx::query(
        "SELECT placement_kind, roster_policy, workspace_id, title, \
         endpoint_a_kind, endpoint_a_id, endpoint_a_deleted_at, \
         endpoint_b_kind, endpoint_b_id, endpoint_b_deleted_at \
         FROM conversations WHERE id = ?",
    )
    .bind(conversation.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(AgentReadHistoryStoreError::from)?
    else {
        return Ok(Err(ReadHistoryRefusal::ConversationNotFound));
    };

    let placement_kind_raw: String = row
        .try_get("placement_kind")
        .map_err(AgentReadHistoryStoreError::from)?;
    let placement_kind = ConversationPlacementKind::parse(&placement_kind_raw).ok_or(
        AgentReadHistoryStoreError::CorruptRow("conversations.placement_kind"),
    )?;
    let roster_raw: String = row
        .try_get("roster_policy")
        .map_err(AgentReadHistoryStoreError::from)?;
    let roster_policy = ConversationRosterPolicy::parse(&roster_raw).ok_or(
        AgentReadHistoryStoreError::CorruptRow("conversations.roster_policy"),
    )?;
    let workspace_raw: Option<String> = row
        .try_get("workspace_id")
        .map_err(AgentReadHistoryStoreError::from)?;
    let workspace_id = workspace_raw
        .map(WorkspaceId::parse)
        .transpose()
        .map_err(|_| AgentReadHistoryStoreError::CorruptRow("conversations.workspace_id"))?;

    let participates = match placement_kind {
        ConversationPlacementKind::Workspace => {
            let hit: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM conversation_agents WHERE conversation_id = ? AND agent_id = ?",
            )
            .bind(conversation.as_str())
            .bind(agent.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(AgentReadHistoryStoreError::from)?;
            hit.is_some()
        }
        ConversationPlacementKind::DirectPair => {
            let mut participates = false;
            for (kind_col, id_col, deleted_col) in [
                ("endpoint_a_kind", "endpoint_a_id", "endpoint_a_deleted_at"),
                ("endpoint_b_kind", "endpoint_b_id", "endpoint_b_deleted_at"),
            ] {
                let kind: Option<String> = row
                    .try_get(kind_col)
                    .map_err(AgentReadHistoryStoreError::from)?;
                let id: Option<String> = row
                    .try_get(id_col)
                    .map_err(AgentReadHistoryStoreError::from)?;
                let deleted: Option<i64> = row
                    .try_get(deleted_col)
                    .map_err(AgentReadHistoryStoreError::from)?;
                if kind.as_deref() == Some("agent")
                    && id.as_deref() == Some(agent.as_str())
                    && deleted.is_none()
                {
                    participates = true;
                }
            }
            participates
        }
    };
    if !participates {
        return Ok(Err(ReadHistoryRefusal::NotParticipant));
    }

    if let Some(workspace) = &workspace_id {
        let granted: Option<i64> = sqlx::query_scalar(
            "SELECT CASE WHEN w.owner_agent_id = ? OR EXISTS (SELECT 1 FROM \
             workspace_agent_capabilities c WHERE c.workspace_id = w.id AND c.agent_id = ? \
             AND c.capability = 'read_messages') THEN 1 ELSE 0 END \
             FROM workspaces w WHERE w.id = ?",
        )
        .bind(agent.as_str())
        .bind(agent.as_str())
        .bind(workspace.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(AgentReadHistoryStoreError::from)?;
        if !granted.is_some_and(|allowed| allowed != 0) {
            return Ok(Err(ReadHistoryRefusal::CapabilityDenied));
        }
    }

    let title: String = row
        .try_get("title")
        .map_err(AgentReadHistoryStoreError::from)?;

    Ok(Ok(MessageTargetCoordinates {
        conversation: conversation.clone(),
        placement_kind,
        roster_policy,
        channel_id: conversation.clone(),
        workspace_id,
        title,
    }))
}

/// The window's reaction aggregates as ONE bulk snapshot read,
/// with `viewer_reacted` computed for THIS agent, keyed by message id. Each message's
/// JSON is `[{emoji,count,viewer_reacted}]` ordered by first-reaction time then emoji
/// (the shared `aggregate_facts_in_snapshot` already returns that order); a message
/// with no reactions is absent (→ `reactions_json = None`).
async fn reaction_aggregates_for(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    message_ids: &[MessageId],
) -> Result<HashMap<String, serde_json::Value>, PersistenceFailure> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let viewer = Reactor::Agent(agent.clone());
    let facts = aggregate_facts_in_snapshot(&mut *tx, Some(&viewer), message_ids)
        .await
        .map_err(|_| AgentReadHistoryStoreError::Reaction)?;
    let mut grouped: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    for fact in &facts {
        grouped
            .entry(fact.message.as_str().to_owned())
            .or_default()
            .push(serde_json::json!({
                "emoji": fact.aggregate.emoji().as_str(),
                "count": fact.aggregate.count().value(),
                "viewer_reacted": fact.aggregate.viewer_reacted(),
            }));
    }
    Ok(grouped
        .into_iter()
        .map(|(id, arr)| (id, serde_json::Value::Array(arr)))
        .collect())
}

/// Gate a persisted numeric through the JS-safe integer domain at store→projection:
/// a stored value JS cannot represent exactly is a read error
/// (`CorruptRow`), not a silently-truncated wire number.
fn js_safe(value: i64, column: &'static str) -> Result<i64, PersistenceFailure> {
    silverhand_backend_core::foundation::wire_number::JsSafeInteger::new(value)
        .map(|v| v.get())
        .map_err(|_| {
            PersistenceFailure::from_source(AgentReadHistoryStoreError::CorruptRow(column))
        })
}

fn first_seq(rows: &[RawRow]) -> Option<i64> {
    rows.first().map(|row| row.seq)
}
fn last_seq(rows: &[RawRow]) -> Option<i64> {
    rows.last().map(|row| row.seq)
}

fn thread_scope(thread_root: Option<&str>) -> &'static str {
    if thread_root.is_some() {
        " AND (m.id = ? OR m.thread_root_id = ?)"
    } else {
        ""
    }
}
fn system_scope(include_system: bool) -> &'static str {
    if include_system {
        ""
    } else {
        " AND m.from_kind <> 'system'"
    }
}

/// Bind the optional thread-scope params (root twice) after the leading binds.
fn bind_thread<'q>(
    query: sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments>,
    thread_root: Option<&'q str>,
) -> sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments> {
    match thread_root {
        Some(root) => query.bind(root).bind(root),
        None => query,
    }
}

async fn window_before(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    thread_root: Option<&str>,
    include_system: bool,
    before_seq: i64,
    limit: i64,
) -> Result<Vec<RawRow>, PersistenceFailure> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT * FROM (SELECT {COLUMNS} FROM messages m WHERE m.conversation_id = ? AND m.seq < ?{system}{thread} ORDER BY m.seq DESC LIMIT ?) ORDER BY seq ASC",
        system = system_scope(include_system),
        thread = thread_scope(thread_root),
    );
    let query = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
        .bind(conversation.as_str())
        .bind(before_seq);
    let query = bind_thread(query, thread_root).bind(limit);
    decode_rows(
        query
            .fetch_all(&mut **tx)
            .await
            .map_err(AgentReadHistoryStoreError::from)?,
    )
}

async fn window_after(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    thread_root: Option<&str>,
    include_system: bool,
    after_seq: i64,
    limit: i64,
) -> Result<Vec<RawRow>, PersistenceFailure> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {COLUMNS} FROM messages m WHERE m.conversation_id = ? AND m.seq > ?{system}{thread} ORDER BY m.seq ASC LIMIT ?",
        system = system_scope(include_system),
        thread = thread_scope(thread_root),
    );
    let query = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
        .bind(conversation.as_str())
        .bind(after_seq);
    let query = bind_thread(query, thread_root).bind(limit);
    decode_rows(
        query
            .fetch_all(&mut **tx)
            .await
            .map_err(AgentReadHistoryStoreError::from)?,
    )
}

async fn row_at_seq(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    thread_root: Option<&str>,
    include_system: bool,
    seq: i64,
) -> Result<Option<RawRow>, PersistenceFailure> {
    let sql = format!(
        "SELECT {COLUMNS} FROM messages m WHERE m.conversation_id = ? AND m.seq = ?{system}{thread} LIMIT 1",
        system = system_scope(include_system),
        thread = thread_scope(thread_root),
    );
    let query = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
        .bind(conversation.as_str())
        .bind(seq);
    let query = bind_thread(query, thread_root);
    let rows = decode_rows(
        query
            .fetch_all(&mut **tx)
            .await
            .map_err(AgentReadHistoryStoreError::from)?,
    )?;
    Ok(rows.into_iter().next())
}

async fn anchor_seq(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    thread_root: Option<&str>,
    message_id: &str,
) -> Result<Option<i64>, PersistenceFailure> {
    let sql = format!(
        "SELECT m.seq AS seq FROM messages m WHERE m.id = ? AND m.conversation_id = ?{thread} LIMIT 1",
        thread = thread_scope(thread_root),
    );
    let query = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
        .bind(message_id)
        .bind(conversation.as_str());
    let query = bind_thread(query, thread_root);
    let row = query
        .fetch_optional(&mut **tx)
        .await
        .map_err(AgentReadHistoryStoreError::from)?;
    Ok(match row {
        Some(row) => Some(
            row.try_get("seq")
                .map_err(AgentReadHistoryStoreError::from)?,
        ),
        None => None,
    })
}

/// The ONE has_more edge rule shared by every read-history mode: probe
/// strictly outside the FINAL assembled window's min/max seq. An empty window yields
/// `(false, false)` — matching TS, which reads `rows[0]`/`rows.at(-1)` (null on empty)
/// after all four modes. No mode derives an edge from an anchor seq; the window itself
/// is the sole boundary, so system-filter/thread-scope share it automatically.
async fn window_edges(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    thread_root: Option<&str>,
    include_system: bool,
    rows: &[RawRow],
) -> Result<(bool, bool), PersistenceFailure> {
    let more_before = probe_before(
        tx,
        conversation,
        thread_root,
        include_system,
        first_seq(rows),
    )
    .await?;
    let more_after = probe_after(
        tx,
        conversation,
        thread_root,
        include_system,
        last_seq(rows),
    )
    .await?;
    Ok((more_before, more_after))
}

/// Whether an eligible row exists strictly BEFORE the window's first seq.
async fn probe_before(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    thread_root: Option<&str>,
    include_system: bool,
    first_seq: Option<i64>,
) -> Result<bool, PersistenceFailure> {
    let Some(first) = first_seq else {
        return Ok(false);
    };
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM messages m WHERE m.conversation_id = ? AND m.seq < ?{system}{thread})",
        system = system_scope(include_system),
        thread = thread_scope(thread_root),
    );
    let query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.clone()))
        .bind(conversation.as_str())
        .bind(first);
    let query = bind_thread_scalar(query, thread_root);
    Ok(query
        .fetch_one(&mut **tx)
        .await
        .map_err(AgentReadHistoryStoreError::from)?
        != 0)
}

/// Whether an eligible row exists strictly AFTER the window's last seq.
async fn probe_after(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    thread_root: Option<&str>,
    include_system: bool,
    last_seq: Option<i64>,
) -> Result<bool, PersistenceFailure> {
    let Some(last) = last_seq else {
        return Ok(false);
    };
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM messages m WHERE m.conversation_id = ? AND m.seq > ?{system}{thread})",
        system = system_scope(include_system),
        thread = thread_scope(thread_root),
    );
    let query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.clone()))
        .bind(conversation.as_str())
        .bind(last);
    let query = bind_thread_scalar(query, thread_root);
    Ok(query
        .fetch_one(&mut **tx)
        .await
        .map_err(AgentReadHistoryStoreError::from)?
        != 0)
}

fn bind_thread_scalar<'q>(
    query: sqlx::query::QueryScalar<'q, Sqlite, i64, sqlx::sqlite::SqliteArguments>,
    thread_root: Option<&'q str>,
) -> sqlx::query::QueryScalar<'q, Sqlite, i64, sqlx::sqlite::SqliteArguments> {
    match thread_root {
        Some(root) => query.bind(root).bind(root),
        None => query,
    }
}

fn decode_rows(rows: Vec<sqlx::sqlite::SqliteRow>) -> Result<Vec<RawRow>, PersistenceFailure> {
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let from_kind_raw: String = row
            .try_get("from_kind")
            .map_err(AgentReadHistoryStoreError::from)?;
        let from_kind = match from_kind_raw.as_str() {
            "user" => FromKind::User,
            "agent" => FromKind::Agent,
            "system" => FromKind::System,
            _ => return Err(AgentReadHistoryStoreError::UnknownFromKind.into()),
        };
        out.push(RawRow {
            id: row
                .try_get("id")
                .map_err(AgentReadHistoryStoreError::from)?,
            conversation_id: row
                .try_get("conversation_id")
                .map_err(AgentReadHistoryStoreError::from)?,
            content: row
                .try_get("content")
                .map_err(AgentReadHistoryStoreError::from)?,
            metadata_json: row
                .try_get("metadata")
                .map_err(AgentReadHistoryStoreError::from)?,
            seq: js_safe(
                row.try_get("seq")
                    .map_err(AgentReadHistoryStoreError::from)?,
                "messages.seq",
            )?,
            created_at: js_safe(
                row.try_get("created_at")
                    .map_err(AgentReadHistoryStoreError::from)?,
                "messages.created_at",
            )?,
            thread_root_id: row
                .try_get("thread_root_id")
                .map_err(AgentReadHistoryStoreError::from)?,
            evidence: AuthorEvidence {
                from_kind,
                from_agent_id: row
                    .try_get("from_agent_id")
                    .map_err(AgentReadHistoryStoreError::from)?,
                from_user_id: row
                    .try_get("from_user_id")
                    .map_err(AgentReadHistoryStoreError::from)?,
                author_id: row
                    .try_get("author_id")
                    .map_err(AgentReadHistoryStoreError::from)?,
                author_name: row
                    .try_get("author_name")
                    .map_err(AgentReadHistoryStoreError::from)?,
                author_display_name: row
                    .try_get("author_display_name")
                    .map_err(AgentReadHistoryStoreError::from)?,
            },
        });
    }
    Ok(out)
}

/// The attachments array from the message metadata JSON, re-serialized as the wire
/// `AttachmentSnapshot[]` (id/filename/size/content_type/label). `None` when the
/// message has no metadata or no attachments key.
/// Decode the message's stored metadata, FAIL-CLOSED: a stored blob that
/// is not valid JSON is a read error (`CorruptRow`), NOT a silent `null` that
/// disguises persistence corruption as a legitimately-empty message. `None` (no
/// metadata) stays `None`.
fn decode_metadata(
    metadata: Option<&str>,
) -> Result<Option<serde_json::Value>, PersistenceFailure> {
    match metadata {
        None => Ok(None),
        Some(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(value) => Ok(Some(value)),
            Err(_) => Err(PersistenceFailure::from_source(
                AgentReadHistoryStoreError::CorruptRow("messages.metadata"),
            )),
        },
    }
}

/// Extract the STRICT attachment snapshot list from already-decoded metadata,
/// mirroring TS `extractAttachmentSnapshots`: each item must carry a
/// string `id`, string `filename`, a `size` that is ANY JSON number (TS
/// `z.number()` — a fractional size is kept, not dropped), and a string
/// `content_type`; a malformed item is DROPPED. A `label` is kept ONLY if, after
/// `trim()`, it matches the ONE shared authority `^(Image|File) #([1-9]\d{0,3})$`
/// (capitalized kind + ` #` + ordinal 1..=9999) — carried verbatim (untrimmed) when
/// valid. Returns the canonical array `Value`, or `Null` when there are no valid
/// snapshots (TS emits `attachments: null`, NOT `[]`).
fn attachment_snapshots(metadata: Option<&serde_json::Value>) -> serde_json::Value {
    let Some(items) = metadata
        .and_then(|m| m.as_object())
        .and_then(|o| o.get("attachments"))
        .and_then(|a| a.as_array())
    else {
        return serde_json::Value::Null;
    };
    let mut snapshots = Vec::new();
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        let (Some(id), Some(filename), Some(size), Some(content_type)) = (
            object.get("id").and_then(serde_json::Value::as_str),
            object.get("filename").and_then(serde_json::Value::as_str),
            // `size` is ANY JSON number (TS z.number()): keep the number verbatim,
            // including a fractional value TS would accept (as_i64 would drop it).
            object.get("size").filter(|v| v.is_number()),
            object
                .get("content_type")
                .and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        let mut snapshot = serde_json::Map::new();
        snapshot.insert("id".to_owned(), serde_json::Value::String(id.to_owned()));
        snapshot.insert(
            "filename".to_owned(),
            serde_json::Value::String(filename.to_owned()),
        );
        snapshot.insert("size".to_owned(), size.clone());
        snapshot.insert(
            "content_type".to_owned(),
            serde_json::Value::String(content_type.to_owned()),
        );
        // A label is kept ONLY if, after trim, it is canonical — and then the CANONICAL
        // FORMATTED form is emitted, mirroring TS
        // `formatAttachmentLabel(parseAttachmentLabel(label.trim()))`: surrounding
        // whitespace is stripped, so `"  Image #1  "` → `"Image #1"`. NOT verbatim.
        //
        // The trim is the ECMAScript `String.prototype.trim` whitespace set
        // via `is_ecmascript_trim_whitespace`, NOT Rust's `str::trim` (Unicode
        // White_Space): the two DIFFER at the boundary — U+FEFF (ZWNBSP/BOM) IS trimmed
        // by ES but is NOT Rust-White_Space (so `str::trim` would keep it, leaving a
        // non-canonical label), and U+0085 (NEL) is Rust-White_Space but is NOT in the
        // ES trim set (so `str::trim` would strip it, wrongly canonicalizing a label the
        // TS reference keeps verbatim). Matching the TS authority is the only correct
        // classification.
        if let Some(raw_label) = object.get("label").and_then(serde_json::Value::as_str) {
            let trimmed = raw_label.trim_matches(is_ecmascript_trim_whitespace);
            if is_canonical_attachment_label(trimmed) {
                snapshot.insert(
                    "label".to_owned(),
                    serde_json::Value::String(trimmed.to_owned()),
                );
            }
        }
        snapshots.push(serde_json::Value::Object(snapshot));
    }
    if snapshots.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Array(snapshots)
    }
}

/// The ONE shared attachment-label authority `^(Image|File) #([1-9]\d{0,3})$`:
/// a capitalized `Image`/`File`, a literal ` #`, then an ordinal with no
/// leading zero, 1..=4 digits (1..=9999). The caller trims first. Any other string is
/// treated as absent (positional fallback).
fn is_canonical_attachment_label(label: &str) -> bool {
    let Some(rest) = label
        .strip_prefix("Image #")
        .or_else(|| label.strip_prefix("File #"))
    else {
        return false;
    };
    // Ordinal: 1..=4 digits, no leading zero (matches `[1-9]\d{0,3}`).
    let bytes = rest.as_bytes();
    if bytes.is_empty() || bytes.len() > 4 || bytes[0] == b'0' {
        return false;
    }
    bytes.iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str = include_str!(
        "../../../../../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    );

    // A regular workspace `ws` with channel `chan`; agent-a is a member + has
    // read_messages; agent-b is a member WITHOUT read_messages; agent-x is not a
    // member. One message `m1` at seq 1 with agent-a's reaction. Plus a direct_pair
    // `dm` (endpoint agent-a).
    const SEED: &str = "\
INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('u1','u','U',1,1); \
INSERT INTO hosts (id,name,kind,created_at) VALUES ('h1','H','local',1); \
INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES ('agent-a','alice','Alice','claude','h1','u1',1,1,'conversation'); \
INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES ('agent-b','bob','Bob','codex','h1','u1',1,1,'conversation'); \
INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES ('agent-x','carol','Carol','codex','h1','u1',1,1,'conversation'); INSERT INTO agents (id,name,display_name,runtime,host_id,created_by_user_id,created_at,updated_at,session_scope) VALUES ('agent-c','dave','Dave','codex','h1','u1',1,1,'conversation'); \
INSERT INTO workspaces (id,name,name_key,owner_agent_id,created_at,updated_at) VALUES ('ws','Team','team',NULL,1,1); \
INSERT INTO workspace_members (workspace_id,agent_id,joined_at) VALUES ('ws','agent-a',1); \
INSERT INTO workspace_members (workspace_id,agent_id,joined_at) VALUES ('ws','agent-b',1); INSERT INTO workspace_members (workspace_id,agent_id,joined_at) VALUES ('ws','agent-c',1); \
INSERT INTO workspace_agent_capabilities (workspace_id,agent_id,capability) VALUES ('ws','agent-a','read_messages'); INSERT INTO workspace_agent_capabilities (workspace_id,agent_id,capability) VALUES ('ws','agent-b','read_messages'); \
INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES ('chan','ws','workspace','open','Chan','chan','idle','private',1,1); \
INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('chan','agent-a',1); \
INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('chan','agent-b',1); INSERT INTO conversation_agents (conversation_id,agent_id,joined_at) VALUES ('chan','agent-c',1); \
INSERT INTO messages (id,conversation_id,from_kind,content,metadata,seq,created_at) VALUES ('m1','chan','agent','hello',NULL,1,10); \
INSERT INTO message_reactions (id,message_id,reactor_kind,reactor_id,emoji,created_at) VALUES ('r1','m1','agent','agent-a','👍',5); \
INSERT INTO message_reactions (id,message_id,reactor_kind,reactor_id,emoji,created_at) VALUES ('r2','m1','agent','agent-b','🎉',6); \
INSERT INTO conversations (id,placement_kind,roster_policy,title,status,visibility,created_at,updated_at,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id) VALUES ('dm','direct_pair','fixed','DM','idle','private',1,1,'agent','agent-a','human','u1');";

    async fn seeded() -> (tempfile::TempDir, SqliteAgentReadHistory) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rh.sqlite");
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
        (dir, SqliteAgentReadHistory::from_pool(pool))
    }
    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).unwrap()
    }
    fn conv(id: &str) -> ConversationId {
        ConversationId::parse(id).unwrap()
    }

    #[tokio::test]
    async fn participant_with_cap_reads_full_coords_and_own_reaction_true() {
        let (_d, read) = seeded().await;
        let w = read
            .read_history(
                &agent("agent-a"),
                &conv("chan"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                true,
                50,
            )
            .await
            .unwrap()
            .unwrap();
        // Full target coordinates.
        assert_eq!(
            w.target.placement_kind,
            ConversationPlacementKind::Workspace
        );
        assert_eq!(w.target.roster_policy, ConversationRosterPolicy::Open);
        assert_eq!(w.target.channel_id.as_str(), "chan");
        assert_eq!(w.target.workspace_id.as_ref().unwrap().as_str(), "ws");
        assert_eq!(w.target.title, "Chan");
        assert_eq!(w.messages.len(), 1);
        // Reaction aggregate: agent-a's 👍 → viewer_reacted true; agent-b's 🎉 → false.
        let reactions = w.messages[0].reactions.clone();
        let arr = reactions.as_array().unwrap();
        let thumbs = arr.iter().find(|r| r["emoji"] == "👍").unwrap();
        assert_eq!(thumbs["viewer_reacted"], true);
        let party = arr.iter().find(|r| r["emoji"] == "🎉").unwrap();
        assert_eq!(party["viewer_reacted"], false);
    }

    #[tokio::test]
    async fn other_agent_reaction_is_false() {
        let (_d, read) = seeded().await;
        let w = read
            .read_history(
                &agent("agent-b"),
                &conv("chan"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                true,
                50,
            )
            .await
            .unwrap()
            .unwrap();
        let reactions = w.messages[0].reactions.clone();
        let thumbs = reactions
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["emoji"] == "👍")
            .unwrap();
        assert_eq!(thumbs["viewer_reacted"], false, "agent-b did not 👍");
        let party = reactions
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["emoji"] == "🎉")
            .unwrap();
        assert_eq!(party["viewer_reacted"], true, "agent-b did 🎉");
    }

    #[tokio::test]
    async fn in_snapshot_refusals() {
        let (_d, read) = seeded().await;
        // missing conversation.
        assert!(matches!(
            read.read_history(
                &agent("agent-a"),
                &conv("nope"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                false,
                50
            )
            .await
            .unwrap(),
            Err(ReadHistoryRefusal::ConversationNotFound)
        ));
        // non-participant (agent-x).
        assert!(matches!(
            read.read_history(
                &agent("agent-x"),
                &conv("chan"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                false,
                50
            )
            .await
            .unwrap(),
            Err(ReadHistoryRefusal::NotParticipant)
        ));
        // participant without read_messages (agent-b).
        assert!(matches!(
            read.read_history(
                &agent("agent-c"),
                &conv("chan"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                false,
                50
            )
            .await
            .unwrap(),
            Err(ReadHistoryRefusal::CapabilityDenied)
        ));
    }

    #[tokio::test]
    async fn direct_pair_has_no_capability_axis_and_null_workspace() {
        let (_d, read) = seeded().await;
        let w = read
            .read_history(
                &agent("agent-a"),
                &conv("dm"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                false,
                50,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            w.target.placement_kind,
            ConversationPlacementKind::DirectPair
        );
        assert!(w.target.workspace_id.is_none(), "DM ref has no workspace");
    }

    #[tokio::test]
    async fn revoke_read_messages_after_a_read_flips_the_next_read_closed() {
        let (dir, read) = seeded().await;
        // First read authorized.
        assert!(
            read.read_history(
                &agent("agent-a"),
                &conv("chan"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                false,
                50
            )
            .await
            .unwrap()
            .is_ok()
        );
        // Revoke the capability (a competing writer), then the NEXT read is refused —
        // authority is replayed in-snapshot, not cached from the earlier read.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.path().join("rh.sqlite"))
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id='ws' AND agent_id='agent-a'")
            .execute(&pool).await.unwrap();
        assert!(matches!(
            read.read_history(
                &agent("agent-a"),
                &conv("chan"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                false,
                50
            )
            .await
            .unwrap(),
            Err(ReadHistoryRefusal::CapabilityDenied)
        ));
    }

    #[tokio::test]
    async fn malformed_metadata_fails_closed_not_fake_null() {
        let (dir, read) = seeded().await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.path().join("rh.sqlite"))
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE messages SET metadata='{not json' WHERE id='m1'")
            .execute(&pool)
            .await
            .unwrap();
        let result = read
            .read_history(
                &agent("agent-a"),
                &conv("chan"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                false,
                50,
            )
            .await;
        assert!(
            result.is_err(),
            "corrupt stored metadata is a read error, not a fake null"
        );
    }

    #[tokio::test]
    async fn js_unsafe_stored_seq_fails_closed_not_a_truncated_wire_number() {
        // #48: a persisted seq beyond MAX_SAFE_INTEGER is a read error at
        // store→projection, never a silently-rounded wire number.
        let (dir, read) = seeded().await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.path().join("rh.sqlite"))
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE messages SET seq = 9007199254740993 WHERE id='m1'")
            .execute(&pool)
            .await
            .unwrap();
        let result = read
            .read_history(
                &agent("agent-a"),
                &conv("chan"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                false,
                50,
            )
            .await;
        assert!(result.is_err(), "JS-unsafe stored seq fails closed");
    }

    #[tokio::test]
    async fn strict_attachment_decoder_label_regex_size_any_number_and_null_when_empty() {
        let (dir, read) = seeded().await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.path().join("rh.sqlite"))
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        // A label with surrounding whitespace is trimmed+matched and
        // emitted in CANONICAL formatted form (`Image #1`); a FRACTIONAL size (TS
        // z.number()) is kept, not dropped; a malformed item is dropped.
        let meta = r#"{"attachments":[{"id":"a1","filename":"f.png","size":1.5,"content_type":"image/png","label":"  Image #1  "},{"id":"bad","filename":123}]}"#;
        sqlx::query("UPDATE messages SET metadata=? WHERE id='m1'")
            .bind(meta)
            .execute(&pool)
            .await
            .unwrap();
        let w = read
            .read_history(
                &agent("agent-a"),
                &conv("chan"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                false,
                50,
            )
            .await
            .unwrap()
            .unwrap();
        let attachments = w.messages[0].attachments.clone();
        let arr = attachments.as_array().unwrap();
        assert_eq!(arr.len(), 1, "malformed item dropped");
        assert_eq!(arr[0]["id"], "a1");
        assert_eq!(
            arr[0]["size"],
            serde_json::json!(1.5),
            "fractional size kept"
        );
        assert_eq!(
            arr[0]["label"], "Image #1",
            "a trim-matching label is emitted in CANONICAL formatted form (surrounding whitespace stripped), NOT verbatim"
        );

        // A message with NO valid attachments projects `attachments: null` (NOT []).
        sqlx::query(r#"UPDATE messages SET metadata='{"attachments":[]}' WHERE id='m1'"#)
            .execute(&pool)
            .await
            .unwrap();
        let w2 = read
            .read_history(
                &agent("agent-a"),
                &conv("chan"),
                None,
                &ReadHistoryAnchor::Head,
                true,
                false,
                50,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(w2.messages[0].attachments, serde_json::Value::Null);

        for (label, expected) in [
            ("Image #1", Some("Image #1")),
            ("File #9999", Some("File #9999")),
            ("image 1", None),
            ("Image #01", None),
            ("Image #10000", None),
            ("Image #0", None),
            ("Photo #1", None),
        ] {
            let metadata = serde_json::json!({"attachments": [{
                "id": "a1", "filename": "f.png", "size": 1.5,
                "content_type": "image/png", "label": label,
            }]});
            sqlx::query("UPDATE messages SET metadata=? WHERE id='m1'")
                .bind(metadata.to_string())
                .execute(&pool)
                .await
                .unwrap();
            let window = read
                .read_history(
                    &agent("agent-a"),
                    &conv("chan"),
                    None,
                    &ReadHistoryAnchor::Head,
                    true,
                    false,
                    50,
                )
                .await
                .unwrap()
                .unwrap();
            let attachments = window.messages[0].attachments.as_array().unwrap();
            assert_eq!(
                attachments.len(),
                1,
                "invalid label preserves the attachment: {label}"
            );
            assert_eq!(attachments[0]["id"], "a1");
            assert_eq!(
                attachments[0].get("label"),
                expected.map(serde_json::Value::from).as_ref(),
                "{label}"
            );
        }
    }

    #[test]
    fn attachment_snapshots_replay_the_ts_signed_parity_fixture() {
        // The Rust extractor reproduces the TS extractAttachmentSnapshots
        // output cell-for-cell (canonical FORMATTED label, any-number size, malformed
        // dropped, empty -> null), per the fixture the TS signer regenerates.
        #[derive(serde::Deserialize)]
        struct Cell {
            label: String,
            metadata: serde_json::Value,
            attachments: serde_json::Value,
        }
        #[derive(serde::Deserialize)]
        struct Fixture {
            cells: Vec<Cell>,
        }
        const FIXTURE: &str =
            include_str!("../../../../tests/fixtures/attachment-snapshots.parity.json");
        let fixture: Fixture = serde_json::from_str(FIXTURE).unwrap();
        for cell in fixture.cells {
            let produced = attachment_snapshots(Some(&cell.metadata));
            assert_eq!(produced, cell.attachments, "cell {}", cell.label);
        }
    }
}
