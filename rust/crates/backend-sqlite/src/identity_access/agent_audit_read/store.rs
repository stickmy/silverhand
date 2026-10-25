//! The `BEGIN DEFERRED` source-bound projection behind [`SqliteAgentAuditRead`], moved
//! out of the namespace `mod.rs` (which declares and re-exports only).
//!
//! ## One snapshot carries facts AND data, gated before the data is read
//!
//! Each face reads, on ONE deferred transaction, the subject/participation facts its
//! gate is decided against, decides the FULL owner floor on those freshly-read facts
//! ([`audit_chats_floor`] / [`audit_conversation_floor`] — source-bind, then active-stage
//! `can_view_agent`, then the `capability::agent_audit` owner rule), and reads the data
//! it projects ONLY when that floor allows. So a caller who lost visibility (the agent
//! left its Active stage) or ownership between Stage A and this read reaches zero rows,
//! and a fault in a data row of a target they may not audit can never be rewritten into a
//! read 500. The route re-runs the SAME shared floor on the bundle's own facts purely to
//! MAP the fresh verdict to the wire (404 vs 403 vs safe 500) — never to reach the data.
//! There is no window between the gate and the read for the facts to change under — no
//! TOCTOU and no ACL/data splice. The cheap owner floor still runs first
//! ([`read_audit_subject`](AgentAuditReadModel::read_audit_subject)), so a stranger never
//! reaches these data reads at all.
//!
//! ## Every reused authority is the ONE authority
//!
//! The subject is decoded through [`crate::identity_access::agent_detail_read::decode_agent_row`] and
//! its audience through [`crate::identity_access::agent_view_acl::read_agent_audience`] — the SAME facts
//! the agent-detail and availability faces are gated against, so the audit owner floor
//! cannot drift from them. The transcript decodes and seals rows through
//! [`crate::conversation_messaging::messaging::collaboration::conversation_message_window::RawMessage`] and
//! resolves authors through [`crate::conversation_messaging::messaging::author::resolve_authors`], so an audit
//! message means exactly what the authenticated channel window means — but it selects
//! its OWN `mode=all` rows (no mainline/thread filter). The direct-pair counterpart is
//! resolved through [`crate::conversation_messaging::direct_pair_read`]'s `PairEvidence`/`LivePrincipals`, so
//! the audit list and the DM list agree on deleted-vs-active and read-only. The
//! attachment row is loaded through [`crate::conversation_messaging::messaging::collaboration::attachment::load_attachment`]
//! and bound here by `channel == conversation`; the blob store is NEVER touched in the
//! read model.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AttachmentId, MessageSeq,
        collaboration::{
            conversation_message_window::{
                ConversationMessageWindowRequest, MessageWindowAnchor, SelectedWindowMessage,
            },
            conversation_ref::ConversationRosterPolicy,
        },
    },
    foundation::identity::{AgentId, ConversationId, Principal, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::agent_audit_read::{
        AgentAuditReadModel, AuditAttachmentBundle, AuditChatSummary, AuditChatsBundle,
        AuditSubjectFacts, AuditTranscriptBundle, AuditTranscriptWindow, audit_chats_floor,
        audit_conversation_floor,
    },
    identity_access::capability::agent_audit::AuditedConversation,
};
use sqlx::{QueryBuilder, Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::{
    conversation_messaging::direct_pair_read::{DirectPairReadError, LivePrincipals, PairEvidence},
    conversation_messaging::messaging::{
        author::resolve_authors,
        collaboration::{
            attachment::AttachmentReadStoreError,
            conversation_message_window::{
                ConversationMessageWindowStoreError, MESSAGE_COLUMNS, MESSAGE_JOINS, RawMessage,
            },
        },
    },
    identity_access::agent_detail_read::{
        AgentDetailColdReadError, agent_detail_select_from, decode_agent_row,
    },
    identity_access::agent_view_acl::read_agent_audience,
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFERRED_READ_BEGIN: &str = "BEGIN DEFERRED";

/// The subject facts read narrowed to ONE agent id, composed at compile time so sqlx
/// sees a `&'static str` — the same row shape the detail face decodes.
const SELECT_AGENT_SUBJECT_ROW: &str = concat!(agent_detail_select_from!(), " WHERE a.id = ?");

/// The workspace chats the agent is a roster member of. `LEFT JOIN workspaces` (never
/// `INNER`), so a row whose workspace vanished still surfaces — and is caught by the
/// missing-coordinate decode below rather than silently dropped.
const SELECT_WORKSPACE_CHATS: &str = "SELECT c.id, c.roster_policy, c.title, c.workspace_id, \
     w.name AS workspace_name, c.archived_at, c.created_at \
     FROM conversations c LEFT JOIN workspaces w ON w.id = c.workspace_id \
     WHERE c.placement_kind = 'workspace' AND EXISTS( \
       SELECT 1 FROM conversation_agents ca WHERE ca.conversation_id = c.id AND ca.agent_id = ?) \
     ORDER BY c.created_at ASC, c.id ASC";

/// The direct pairs one of whose endpoints is the agent. Selects exactly the columns
/// [`PairEvidence::decode`] reads plus `archived_at`, so the counterpart is resolved
/// through the same authority the DM list uses.
const SELECT_DIRECT_PAIR_CHATS: &str = "SELECT c.id, c.endpoint_a_kind, c.endpoint_a_id, \
     c.endpoint_a_deleted_at, c.endpoint_b_kind, c.endpoint_b_id, c.endpoint_b_deleted_at, \
     c.created_at, c.archived_at FROM conversations c WHERE c.placement_kind = 'direct_pair' \
     AND ((c.endpoint_a_kind = 'agent' AND c.endpoint_a_id = ?) \
       OR (c.endpoint_b_kind = 'agent' AND c.endpoint_b_id = ?)) \
     ORDER BY c.created_at ASC, c.id ASC";

/// The existence and participation facts for ONE conversation the audit gate is decided
/// against, plus the row's OWN `conversations.id` so the bundle coordinate is decoded
/// from storage rather than echoed from the request. `agent_participates` is the SAME
/// predicate the list arms use, evaluated for this one row: workspace roster membership,
/// or a direct-pair endpoint match.
const SELECT_CONVERSATION_FACTS: &str = "SELECT c.id, c.placement_kind, \
     EXISTS(SELECT 1 FROM conversation_agents ca WHERE ca.conversation_id = c.id \
       AND ca.agent_id = ?) AS ws_member, \
     ((c.endpoint_a_kind = 'agent' AND c.endpoint_a_id = ?) \
       OR (c.endpoint_b_kind = 'agent' AND c.endpoint_b_id = ?)) AS dp_member \
     FROM conversations c WHERE c.id = ?";

#[derive(Clone)]
pub struct SqliteAgentAuditRead {
    pool: crate::foundation::database::SqliteDatabase,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. Every variant
/// is a driver fault or corruption of a committed row, never a product-level refusal —
/// those are decisions the core capability makes from the facts this returns.
#[derive(Debug, Error)]
pub enum AgentAuditReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("agent subject decode failed")]
    Subject(#[from] AgentDetailColdReadError),
    #[error("direct-pair counterpart resolution failed")]
    DirectPair(#[from] DirectPairReadError),
    #[error("message-window projection failed")]
    Window(#[from] ConversationMessageWindowStoreError),
    #[error("attachment row decode failed")]
    Attachment(#[from] AttachmentReadStoreError),
    #[error("message author resolution failed")]
    Author(#[source] PersistenceFailure),
    #[error("stored column {column} does not name a usable identity")]
    MalformedIdentity { column: &'static str },
    #[error("stored conversations.roster_policy is outside the closed set")]
    MalformedRosterPolicy,
    #[error("stored conversations.placement_kind is outside the closed set")]
    MalformedPlacement,
    #[error("workspace chat row is missing its workspace coordinate")]
    MissingWorkspaceCoordinate,
}

impl SqliteAgentAuditRead {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AgentAuditReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn subject_transaction(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AuditSubjectFacts>, AgentAuditReadError> {
        let mut tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;
        let subject = read_subject(&mut tx, agent).await?;
        tx.commit().await?;
        Ok(subject)
    }

    async fn chats_transaction(
        &self,
        principal: &Principal,
        agent: &AgentId,
    ) -> Result<Option<AuditChatsBundle>, AgentAuditReadError> {
        let mut tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;
        let Some(subject) = read_subject(&mut tx, agent).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        // The FULL owner floor — source-bind, active-stage visibility, then the owner
        // capability — is decided on this snapshot's facts BEFORE the list is read, so a
        // caller who lost visibility (agent no longer Active) or ownership since Stage A
        // reaches zero chat rows. A chat-row fault can never rewrite that refusal into a
        // read 500; the route maps the same fresh verdict to the wire (404 vs 403).
        let chats = if audit_chats_floor(principal, agent, &subject).is_allowed() {
            read_chats(&mut tx, agent).await?
        } else {
            Vec::new()
        };
        tx.commit().await?;
        Ok(Some(AuditChatsBundle::from_source(subject, chats)))
    }

    async fn transcript_transaction(
        &self,
        principal: &Principal,
        agent: &AgentId,
        conversation: &ConversationId,
        request: &ConversationMessageWindowRequest,
    ) -> Result<Option<AuditTranscriptBundle>, AgentAuditReadError> {
        let mut tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;
        let Some(subject) = read_subject(&mut tx, agent).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        let source = read_conversation_source(&mut tx, agent, conversation).await?;
        let audited = audited_of(source.as_ref());
        // The window is read ONLY when the FULL floor — source-bind, active-stage
        // visibility, then the owner-conversation capability — allows on these
        // freshly-read facts, so an owner auditing a conversation the agent never joined,
        // OR any caller whose visibility lapsed since Stage A, gets a stable refusal even
        // if one of the conversation's message rows is corrupt.
        let window = if audit_conversation_floor(principal, agent, &subject, audited).is_allowed() {
            read_transcript_window(&mut tx, conversation, request).await?
        } else {
            None
        };
        tx.commit().await?;
        Ok(Some(AuditTranscriptBundle::from_source(
            subject,
            source.map(|source| source.stored_id),
            audited,
            window,
        )))
    }

    async fn attachment_transaction(
        &self,
        principal: &Principal,
        agent: &AgentId,
        conversation: &ConversationId,
        attachment: &AttachmentId,
    ) -> Result<Option<AuditAttachmentBundle>, AgentAuditReadError> {
        let mut tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;
        let Some(subject) = read_subject(&mut tx, agent).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        let source = read_conversation_source(&mut tx, agent, conversation).await?;
        let audited = audited_of(source.as_ref());
        // The attachment row (and later its blob) is read ONLY when the FULL floor —
        // source-bind, active-stage visibility, then the owner-conversation capability —
        // allows, so a denied caller (including one whose visibility lapsed since Stage A)
        // never touches the store. The row is loaded by id and strictly decoded, then
        // bound HERE to the audited conversation by its owning channel: "absent" and
        // "belongs to another conversation" are one `None`, so a guessed id is not an
        // existence oracle.
        let attachment = if audit_conversation_floor(principal, agent, &subject, audited)
            .is_allowed()
        {
            crate::conversation_messaging::messaging::collaboration::attachment::load_attachment(
                &mut tx, attachment,
            )
            .await?
            .filter(|record| record.channel() == conversation)
        } else {
            None
        };
        tx.commit().await?;
        Ok(Some(AuditAttachmentBundle::from_source(
            subject,
            source.map(|source| source.stored_id),
            audited,
            attachment,
        )))
    }
}

/// The audited conversation's decoded storage coordinate and participation fact. Built
/// only for a row that exists, so absence is an explicit `None` rather than a request
/// echo.
struct ConversationSource {
    stored_id: ConversationId,
    agent_participates: bool,
}

/// The gate facts for a conversation source: an existing row contributes its
/// participation fact; absence is `{exists: false}` (never an error).
fn audited_of(source: Option<&ConversationSource>) -> AuditedConversation {
    match source {
        Some(source) => AuditedConversation {
            exists: true,
            agent_participates: source.agent_participates,
        },
        None => AuditedConversation {
            exists: false,
            agent_participates: false,
        },
    }
}

impl AgentAuditReadModel for SqliteAgentAuditRead {
    async fn read_audit_subject(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AuditSubjectFacts>, PersistenceFailure> {
        self.subject_transaction(agent)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_audit_chats(
        &self,
        principal: &Principal,
        agent: &AgentId,
    ) -> Result<Option<AuditChatsBundle>, PersistenceFailure> {
        self.chats_transaction(principal, agent)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_audit_transcript(
        &self,
        principal: &Principal,
        agent: &AgentId,
        conversation: &ConversationId,
        request: &ConversationMessageWindowRequest,
    ) -> Result<Option<AuditTranscriptBundle>, PersistenceFailure> {
        self.transcript_transaction(principal, agent, conversation, request)
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_audit_attachment(
        &self,
        principal: &Principal,
        agent: &AgentId,
        conversation: &ConversationId,
        attachment: &AttachmentId,
    ) -> Result<Option<AuditAttachmentBundle>, PersistenceFailure> {
        self.attachment_transaction(principal, agent, conversation, attachment)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

/// Reads the owner/visibility floor facts off the ONE `agents.id` actually loaded, via
/// the SAME decode + audience the detail and availability faces use. `None` = agent row
/// absent, which the route renders as the anti-enum 404.
async fn read_subject(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
) -> Result<Option<AuditSubjectFacts>, AgentDetailColdReadError> {
    let row = sqlx::query(SELECT_AGENT_SUBJECT_ROW)
        .bind(agent.as_str())
        .fetch_optional(&mut **tx)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let decoded = decode_agent_row(&row)?;
    let audience = read_agent_audience(tx, agent).await?;
    // The creator is passed ONCE: `from_source` seeds the visibility creator arm and is
    // the sole origin of the owner-gate subject, so the human `can_view_agent` admits
    // and the human `can_audit_agent_chats` authorizes are the same value.
    Ok(Some(AuditSubjectFacts::from_source(
        decoded.source,
        decoded.creator,
        audience.direct_pair_human_endpoints,
        audience.shared_workspace_members,
        decoded.materialization_status,
    )))
}

/// Reads every chat the agent takes part in as one discriminated arm per placement, in
/// the SQL's creation order across BOTH arms.
async fn read_chats(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
) -> Result<Vec<AuditChatSummary>, AgentAuditReadError> {
    let mut summaries = Vec::new();

    let workspace_rows = sqlx::query(SELECT_WORKSPACE_CHATS)
        .bind(agent.as_str())
        .fetch_all(&mut **tx)
        .await?;
    for row in &workspace_rows {
        summaries.push(decode_workspace_chat(row)?);
    }

    let direct_rows = sqlx::query(SELECT_DIRECT_PAIR_CHATS)
        .bind(agent.as_str())
        .bind(agent.as_str())
        .fetch_all(&mut **tx)
        .await?;
    let mut pairs = Vec::with_capacity(direct_rows.len());
    for row in &direct_rows {
        let archived_at: Option<i64> = row.try_get("archived_at")?;
        pairs.push((PairEvidence::decode(row)?, archived_at));
    }
    let evidence = pairs
        .iter()
        .map(|(pair, _)| pair.clone())
        .collect::<Vec<_>>();
    let principals = LivePrincipals::load(tx, &evidence).await?;
    for (pair, archived_at) in &pairs {
        let counterpart_endpoint = pair.counterpart_of_agent(agent)?;
        let counterpart = principals.counterpart(counterpart_endpoint)?;
        let read_only = pair
            .both_endpoints()
            .iter()
            .any(|endpoint| endpoint.is_deleted() || !principals.allows_direct_messages(endpoint));
        summaries.push(AuditChatSummary::DirectPair {
            conversation_id: pair.conversation().clone(),
            counterpart,
            read_only,
            archived_at: *archived_at,
            created_at: pair.created_at_ms(),
        });
    }

    // The two arms are read separately because each has its own row shape; the merged
    // order is `created_at ASC, id ASC`, exactly the union order the single-query TS
    // read produced (SQLite TEXT ids compare BINARY, matching `str` byte order).
    summaries.sort_by(|left, right| {
        summary_created_at(left)
            .cmp(&summary_created_at(right))
            .then_with(|| {
                summary_conversation(left)
                    .as_str()
                    .cmp(summary_conversation(right).as_str())
            })
    });
    Ok(summaries)
}

fn decode_workspace_chat(row: &SqliteRow) -> Result<AuditChatSummary, AgentAuditReadError> {
    let conversation_id = conversation_id(row, "id")?;
    let roster_policy =
        ConversationRosterPolicy::parse(&row.try_get::<String, _>("roster_policy")?)
            .map_err(|_| AgentAuditReadError::MalformedRosterPolicy)?;
    // A workspace conversation's CHECK guarantees `workspace_id IS NOT NULL`, but the
    // join to `workspaces` can miss and `name` is only as present as the row is intact:
    // a null coordinate is corruption of a committed row, surfaced as a safe 500, never
    // rendered as an empty workspace.
    let workspace_id_raw: Option<String> = row.try_get("workspace_id")?;
    let workspace_name: Option<String> = row.try_get("workspace_name")?;
    let (workspace_id_raw, workspace_name) = match (workspace_id_raw, workspace_name) {
        (Some(id), Some(name)) => (id, name),
        _ => return Err(AgentAuditReadError::MissingWorkspaceCoordinate),
    };
    let workspace_id = WorkspaceId::parse(workspace_id_raw).map_err(|_| {
        AgentAuditReadError::MalformedIdentity {
            column: "conversations.workspace_id",
        }
    })?;
    Ok(AuditChatSummary::Workspace {
        conversation_id,
        roster_policy,
        title: row.try_get("title")?,
        workspace_id,
        workspace_name,
        archived_at: row.try_get("archived_at")?,
        created_at: row.try_get("created_at")?,
    })
}

fn summary_created_at(summary: &AuditChatSummary) -> i64 {
    match summary {
        AuditChatSummary::Workspace { created_at, .. }
        | AuditChatSummary::DirectPair { created_at, .. } => *created_at,
    }
}

fn summary_conversation(summary: &AuditChatSummary) -> &ConversationId {
    match summary {
        AuditChatSummary::Workspace {
            conversation_id, ..
        }
        | AuditChatSummary::DirectPair {
            conversation_id, ..
        } => conversation_id,
    }
}

/// Reads the source-bound facts for ONE conversation: `None` when no such row exists
/// (which the gate collapses to the one 403 an owner sees for any conversation they may
/// not audit, so a guessed id is not an existence oracle), otherwise the row's OWN
/// decoded `conversations.id` and the same participation predicate the list arms use.
/// The coordinate is decoded from storage, never echoed from the request, so the route's
/// source-bind guard witnesses a real stored id.
async fn read_conversation_source(
    tx: &mut Transaction<'_, Sqlite>,
    agent: &AgentId,
    conversation: &ConversationId,
) -> Result<Option<ConversationSource>, AgentAuditReadError> {
    let row = sqlx::query(SELECT_CONVERSATION_FACTS)
        .bind(agent.as_str())
        .bind(agent.as_str())
        .bind(agent.as_str())
        .bind(conversation.as_str())
        .fetch_optional(&mut **tx)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored_id = conversation_id(&row, "id")?;
    let placement: String = row.try_get("placement_kind")?;
    let agent_participates = match placement.as_str() {
        "workspace" => row.try_get::<i64, _>("ws_member")? != 0,
        "direct_pair" => row.try_get::<i64, _>("dp_member")? != 0,
        _ => return Err(AgentAuditReadError::MalformedPlacement),
    };
    Ok(Some(ConversationSource {
        stored_id,
        agent_participates,
    }))
}

/// Reads the `mode=all` message window — mainline and thread replies interleaved by
/// `seq`, no stream filter — decoded and sealed through the SAME authority the
/// authenticated channel window uses, with EMPTY reactions so every row renders
/// `reactions: null`. `None` = an `around_message_id` anchor is not in the conversation,
/// which the route reveals only AFTER its gate allows.
async fn read_transcript_window(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    request: &ConversationMessageWindowRequest,
) -> Result<Option<AuditTranscriptWindow>, AgentAuditReadError> {
    let limit = usize::from(request.query().limit());
    let raw = match request.query().anchor() {
        MessageWindowAnchor::Latest => {
            let mut rows = fetch_window(tx, conversation, None, None, limit, false).await?;
            rows.reverse();
            rows
        }
        MessageWindowAnchor::Before(seq) => {
            let mut rows = fetch_window(tx, conversation, Some(*seq), None, limit, false).await?;
            rows.reverse();
            rows
        }
        MessageWindowAnchor::After(seq) => {
            fetch_window(tx, conversation, None, Some(*seq), limit, true).await?
        }
        MessageWindowAnchor::Around(anchor) => {
            let Some(anchor_row) = fetch_window_anchor(tx, conversation, anchor.as_str()).await?
            else {
                return Ok(None);
            };
            let before_limit = limit.saturating_sub(1) / 2;
            let mut before = fetch_window(
                tx,
                conversation,
                Some(anchor_row.seq),
                None,
                before_limit,
                false,
            )
            .await?;
            before.reverse();
            let after_limit = limit.saturating_sub(before.len() + 1);
            let after = fetch_window(
                tx,
                conversation,
                None,
                Some(anchor_row.seq),
                after_limit,
                true,
            )
            .await?;
            before.push(anchor_row);
            before.extend(after);
            before
        }
    };

    // Author identities resolve in one batch, exactly as the authenticated window does.
    let evidence = raw.iter().map(|row| row.author.clone()).collect::<Vec<_>>();
    let resolved = resolve_authors(tx, &evidence)
        .await
        .map_err(AgentAuditReadError::Author)?;
    let mut messages: Vec<SelectedWindowMessage> = Vec::with_capacity(raw.len());
    for (row, author) in raw.into_iter().zip(resolved) {
        messages.push(row.seal(author, Vec::new())?);
    }

    // has_more probes read the neighbourhood beyond the returned page. The latest window
    // forces `has_more_after = false` with no probe, matching the authenticated face.
    let has_more_before = match messages.first() {
        Some(edge) => exists_beyond(tx, conversation, edge.projection().seq, false).await?,
        None => false,
    };
    let is_latest = matches!(request.query().anchor(), MessageWindowAnchor::Latest);
    let has_more_after = if is_latest {
        false
    } else {
        match messages.last() {
            Some(edge) => exists_beyond(tx, conversation, edge.projection().seq, true).await?,
            None => false,
        }
    };

    Ok(Some(AuditTranscriptWindow::projected(
        messages,
        has_more_before,
        has_more_after,
    )))
}

async fn fetch_window(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    before: Option<MessageSeq>,
    after: Option<MessageSeq>,
    limit: usize,
    ascending: bool,
) -> Result<Vec<RawMessage>, AgentAuditReadError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(MESSAGE_COLUMNS)
        .push(" FROM messages m ")
        .push(MESSAGE_JOINS)
        .push(" WHERE m.conversation_id=")
        .push_bind(conversation.as_str());
    if let Some(seq) = before {
        builder.push(" AND m.seq<").push_bind(seq.get());
    }
    if let Some(seq) = after {
        builder.push(" AND m.seq>").push_bind(seq.get());
    }
    builder
        .push(if ascending {
            " ORDER BY m.seq ASC LIMIT "
        } else {
            " ORDER BY m.seq DESC LIMIT "
        })
        .push_bind(
            i64::try_from(limit).map_err(|_| AgentAuditReadError::MalformedIdentity {
                column: "messages.limit",
            })?,
        );
    let rows = builder.build().fetch_all(&mut **tx).await?;
    rows.iter()
        .map(RawMessage::decode)
        .collect::<Result<Vec<_>, _>>()
        .map_err(AgentAuditReadError::Window)
}

async fn fetch_window_anchor(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    anchor: &str,
) -> Result<Option<RawMessage>, AgentAuditReadError> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(MESSAGE_COLUMNS)
        .push(" FROM messages m ")
        .push(MESSAGE_JOINS)
        .push(" WHERE m.conversation_id=")
        .push_bind(conversation.as_str())
        .push(" AND m.id=")
        .push_bind(anchor);
    let row = builder.build().fetch_optional(&mut **tx).await?;
    row.as_ref()
        .map(RawMessage::decode)
        .transpose()
        .map_err(AgentAuditReadError::Window)
}

async fn exists_beyond(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    pivot: MessageSeq,
    after: bool,
) -> Result<bool, AgentAuditReadError> {
    let sql = if after {
        "SELECT EXISTS(SELECT 1 FROM messages m WHERE m.conversation_id=? AND m.seq>?)"
    } else {
        "SELECT EXISTS(SELECT 1 FROM messages m WHERE m.conversation_id=? AND m.seq<?)"
    };
    let found: i64 = sqlx::query_scalar(sql)
        .bind(conversation.as_str())
        .bind(pivot.get())
        .fetch_one(&mut **tx)
        .await?;
    Ok(found != 0)
}

fn conversation_id(
    row: &SqliteRow,
    column: &'static str,
) -> Result<ConversationId, AgentAuditReadError> {
    ConversationId::parse(row.try_get::<String, _>(column)?)
        .map_err(|_| AgentAuditReadError::MalformedIdentity { column })
}
