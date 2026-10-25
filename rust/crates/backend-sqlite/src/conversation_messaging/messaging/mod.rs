//! SQLite adapter for the product create-message write unit.
//!
//! This store CONSUMES the product database. It creates no table, runs no
//! migrator, claims no schema version, and refuses to open a database that does
//! not exist: the schema has an owner, and a second one would be a second
//! migration authority.
//!
//! Everything engine-specific stays here — SQL text, PRAGMAs, transaction mode,
//! driver errors. The port returns application facts and refusals, so no driver
//! type reaches the application or the wire.

use crate::conversation_messaging::runtime_delivery::conversation_allows_delivery;
use std::{
    convert::Infallible,
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use std::collections::HashSet;

use silverhand_backend_core::application_orchestration::agent_creation::OnboardingInboxEnsure;
use silverhand_backend_core::conversation_messaging::messaging::post_commit::{
    InboxEnsureOutcome, LiveMessageInboxEnsure, SealedMessageInboxEnsure,
};
use silverhand_backend_core::conversation_messaging::messaging::{
    self, ActiveAuthor, ConversationEndpoint, ConversationFacts, ConversationRefusal,
    CreatedMessage, DirectPairEndpoints, DirectPairMint, DirectPairMinted, DirectPairOpenedPort,
    DirectPairParticipants, EndpointPrincipalReads, EnvironmentUnavailable, FromKind, InvalidSeq,
    MessageAuthor, MessageContent, MessageCreatedFact, MessageId, MessageReference, MessageSeq,
    NotificationId, OpenDirectPairError, ParticipantHandle, PostAuthorityReads, PrincipalIdentity,
    PrincipalKind, ReplyTargetRefusal, SendActor, SendClock, SendDestination, SendEntropy,
    SendMessage, SendMessageError, SendMessageOutcome, SendMessageUnitOfWork, ThreadFollowerSource,
    ThreadListEntry, ThreadReplyFact, ThreadRootId, ThreadRootRefusal,
    attachments::{AttachmentLabel, AttachmentSnapshot, AttachmentSource},
    freshness::{FreshnessGate, FreshnessGuardInvariant, StaleEventId},
    mention_cc::{self, MentionCopyOutcome, MentionCopySkip},
    mint_conversation_id, open_direct_pair,
    post_commit::{
        CommittedMentionRow, DeliveryPlan, EffectFailure, MainDeliveryPlan, MentionIntent,
        MentionedViewerRecord, PostCommitPlan, ReplyAudience,
    },
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use silverhand_backend_core::identity_access::agents::session_scope::AgentSessionScope;
use sqlx::{Row, Sqlite, SqliteConnection, Transaction, sqlite::SqliteRow};
use thiserror::Error;

// `pub(crate)`: the author-evidence decode + resolution authority is reused by the
// agent-audit read adapter (`crate::identity_access::agent_audit_read`), so the audit transcript and
// the authenticated window resolve a message row's author the same way.
pub(crate) mod author;
pub(crate) mod authorities;
pub mod collaboration;
/// INTEGRATION SEAM, not owned canonical: the schedule fire-path adapter is a
/// bounded context of its own, owned under `messaging/schedule/**`. This
/// declaration only wires that subtree into the crate — it defines nothing.
pub mod schedule;
/// INTEGRATION SEAM, not owned canonical: the task board adapter is a bounded
/// context of its own, owned under `messaging/task/**`. This declaration only
/// wires that subtree into the crate — it defines nothing.
pub mod task;
use author::{AuthorEvidence, resolve_authors};
pub use authorities::{SqliteTargetAddress, TargetAddressStoreError};

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";
#[cfg(test)]
use crate::foundation::database::STORE_POOL_CONNECTIONS as POOL_CONNECTIONS;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SELECT_CONVERSATION_FACTS: &str = "SELECT closed_at, archived_at, placement_kind, \
     endpoint_a_kind, endpoint_a_id, endpoint_a_deleted_at, endpoint_b_kind, endpoint_b_id, \
     endpoint_b_deleted_at FROM conversations WHERE id = ?";
const SELECT_AGENT_EXISTS: &str = "SELECT 1 FROM agents WHERE id = ?";
const SELECT_USER_EXISTS: &str = "SELECT 1 FROM users WHERE id = ?";

/// Participation is placement-based, in one text per principal kind: a workspace
/// channel names its participants by roster row, a direct pair by its endpoints.
/// Splitting the arms would let a rosterless placement answer "not a participant"
/// to the principals the placement itself names.
/// The placement-participation predicate an agent satisfies for a conversation `c`
/// (a workspace roster row OR a direct-pair endpoint), as an `AND (...)` tail with the
/// three agent binds unfilled. ONE fragment behind two coordinates: the send gate's
/// point check [`SELECT_AGENT_PARTICIPATES`] (`c.id = ?`) and the task-read set port
/// [`SELECT_AGENT_PARTICIPATING_SET`] (`json_each` over a candidate set). Composing both
/// from this macro makes drift structurally impossible — an edit reaches both statements
/// or neither.
macro_rules! agent_participation_predicate {
    () => {
        "((c.placement_kind = 'workspace' AND EXISTS (SELECT 1 FROM conversation_agents ca \
         WHERE ca.conversation_id = c.id AND ca.agent_id = ?)) OR (c.placement_kind = \
         'direct_pair' AND ((c.endpoint_a_kind = 'agent' AND c.endpoint_a_id = ?) OR \
         (c.endpoint_b_kind = 'agent' AND c.endpoint_b_id = ?))))"
    };
}
// Re-exported at `pub(crate)` so the crate-root `agent_runtime_state_read` adapter
// composes its "all conversations this agent is a party to" list projection from
// the SAME participation predicate the send gate and task-read set use, rather than
// a second SQL spelling. Grammar unchanged; this only widens reach. The set port
// [`SELECT_AGENT_PARTICIPATING_SET`] is a candidate-set narrowing, not a full
// enumeration, so it cannot stand in for the list read — only the shared predicate can.
pub(crate) use agent_participation_predicate;

pub(crate) const SELECT_AGENT_PARTICIPATES: &str = concat!(
    "SELECT 1 FROM conversations c WHERE c.id = ? AND ",
    agent_participation_predicate!()
);
/// The set-shaped form of the participation read: `json_each(?)` drives the candidate
/// conversation set (one bound JSON array, no dynamic SQL), each id joined to its
/// conversation and probed against the SAME participation tail. Returns the participating
/// SUBSET's ids; the task-read core re-binds each candidate against it, so the `json_each`
/// set is a coordinate narrowing, never a witness.
pub(crate) const SELECT_AGENT_PARTICIPATING_SET: &str = concat!(
    "SELECT c.id FROM json_each(?) requested JOIN conversations c ON c.id = requested.value \
     WHERE ",
    agent_participation_predicate!()
);
/// The runtime-start dispatch spec on the participation predicate: participation gate,
/// the row-owned scope arm (`a.session_scope`), the read-derived presentation origin
/// (`c.id AS origin`, never the request candidate echoed back), the full agent spec,
/// and the scope-matched CLI-session slot are all one atomic projection over a single
/// read transaction. `c.id = ?` is the candidate point form; the predicate binds the
/// agent three times; the trailing `WHERE a.id = ?` re-binds the agent. A non-participant
/// matches no row → `None` → zero wire.
pub(crate) const SELECT_START_DISPATCH_SPEC: &str = concat!(
    "SELECT a.name, a.runtime, a.model, a.reasoning_effort, a.fast_mode, \
     a.host_id, a.session_scope, s.cli_session_id, c.id AS origin \
       FROM agents a \
       JOIN conversations c ON c.id = ? AND ",
    agent_participation_predicate!(),
    " LEFT JOIN agent_runtime_slots s ON s.agent_id = a.id \
        AND ((a.session_scope = 'agent' AND s.conversation_id IS NULL) \
          OR (a.session_scope = 'conversation' AND s.conversation_id = c.id)) \
      WHERE a.id = ?"
);
/// The human arm carries two facts for a channel — the seat AND workspace
/// membership — so a seat that outlived a workspace removal admits nobody.
pub(crate) const SELECT_USER_PARTICIPATES: &str = "SELECT 1 FROM conversations c WHERE c.id = ? AND \
     ((c.placement_kind = 'workspace' AND EXISTS (SELECT 1 FROM conversation_users cu WHERE \
     cu.conversation_id = c.id AND cu.user_id = ?) AND EXISTS (SELECT 1 FROM workspace_users wu \
     WHERE wu.workspace_id = c.workspace_id AND wu.user_id = ?)) OR (c.placement_kind = \
     'direct_pair' AND ((c.endpoint_a_kind = 'human' AND c.endpoint_a_id = ?) OR \
     (c.endpoint_b_kind = 'human' AND c.endpoint_b_id = ?))))";

const SELECT_MESSAGE_IN_CONVERSATION: &str = "SELECT id, thread_root_id, from_kind, \
     from_agent_id, from_user_id FROM messages WHERE conversation_id = ? AND id = ?";
/// The onboarding inbox-ensure's message witness: the row must belong to THIS conversation
/// (the `WHERE`) AND be a genuine onboarding system message. It returns exactly the two
/// columns that discriminate the shape — `from_kind` and the metadata blob — which the ensure
/// path decodes through the ONE metadata grammar and judges through the domain's ONE shape
/// recognizer. No unused columns ride along, and no ordinary same-conversation message can
/// satisfy it.
const SELECT_ONBOARDING_MESSAGE_SHAPE_IN_CONVERSATION: &str =
    "SELECT from_kind, metadata FROM messages WHERE conversation_id = ? AND id = ?";
const SELECT_AGENT_ID_BY_NAME: &str = "SELECT id FROM agents WHERE name = ?";
const SELECT_CONVERSATION_PLACE: &str =
    "SELECT workspace_id, title FROM conversations WHERE id = ?";
const SELECT_WORKSPACE_NAME: &str = "SELECT name FROM workspaces WHERE id = ?";
/// Who, among the names a body used, may read that conversation right now.
///
/// ONE set-shaped read bounded by the handles the message actually named. The
/// adapter holds SQLite's single writer while it runs and a 64KiB body can name
/// many people, so asking per handle would put an unbounded number of point
/// queries under that reservation; `json_each` makes the named set the driver and
/// `users` a unique-index probe per name.
///
/// The human rule is the same two facts as everywhere — the `conversation_users`
/// seat AND workspace membership — conjoined on each candidate's own id. Both
/// halves are load-bearing: a seat that outlived its membership is not a
/// participant, so it must not be told.
///
/// NO PLACEMENT ARM, and its absence is a fact about the caller rather than a
/// simplification: a mention intent carries a `WorkspaceId`, and one is minted
/// only for a workspace-placed conversation (`mention_self` is workspace-only —
/// in a direct pair the stream IS the mention). A rosterless placement reaching
/// here would produce no seat rows and therefore no viewers, so the omission is
/// fail-closed in both readings.
///
/// Membership is checked against the workspace the ROW WILL NAME — the intent's,
/// bound here — not against whatever the conversation column says at record time.
/// Otherwise the entitlement answered for one workspace and the coordinate written
/// pointed at another.
///
/// The author is excluded in the statement, so a person who named themselves is
/// never a candidate. Bound as a nullable id: an agent author excludes nobody,
/// which is what `IS NOT NULL` gives for free.
macro_rules! entitled_viewers_sql {
    () => {
        "SELECT u.id, u.name FROM json_each(?) named JOIN users u ON u.name = named.value WHERE \
         u.id IS NOT ? AND EXISTS (SELECT 1 FROM conversation_users cu WHERE cu.conversation_id = \
         ? AND cu.user_id = u.id) AND EXISTS (SELECT 1 FROM workspace_users wu WHERE \
         wu.workspace_id = ? AND wu.user_id = u.id) ORDER BY u.created_at ASC, u.id ASC"
    };
}

const SELECT_ENTITLED_VIEWERS: &str = entitled_viewers_sql!();
/// The same statement, asked about its own plan, so the bound cannot be asserted
/// against a paraphrase.
#[cfg(test)]
const EXPLAIN_ENTITLED_VIEWERS: &str = concat!("EXPLAIN QUERY PLAN ", entitled_viewers_sql!());

/// The durable record that a person was named in a conversation they can read. Its
/// coordinates are the source's, because that is what the reader can open.
const INSERT_MENTION_SELF: &str = "INSERT INTO notifications (id, kind, user_id, workspace_id, \
     conversation_id, message_id, source_workspace_id, source_conversation_id, source_message_id, \
     payload, created_at) VALUES (?, 'mention_self', ?, ?, ?, ?, ?, ?, ?, ?, ?)";

/// The conversation's agent participants, as the placement names them, conjoined
/// with a live principal row: a delivery row for an agent that no longer exists
/// cannot be written at all. Ordered so the plan does not depend on scan order.
const SELECT_PARTICIPANT_AGENTS: &str = "SELECT a.id FROM conversations c JOIN agents a ON \
     ((c.placement_kind = 'workspace' AND EXISTS (SELECT 1 FROM conversation_agents ca WHERE \
     ca.conversation_id = c.id AND ca.agent_id = a.id)) OR (c.placement_kind = 'direct_pair' AND \
     ((c.endpoint_a_kind = 'agent' AND c.endpoint_a_id = a.id) OR (c.endpoint_b_kind = 'agent' AND \
     c.endpoint_b_id = a.id)))) WHERE c.id = ? ORDER BY a.id ASC";

/// A direct line is got-or-created: the conflict target is the partial unique index
/// over the endpoint columns, so two sends racing to mention the same agent end up
/// on one line rather than two.
const OPEN_DIRECT_PAIR: &str = "INSERT INTO conversations (id, workspace_id, placement_kind, \
     roster_policy, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, title, status, \
     visibility, created_at, updated_at) VALUES (?, NULL, 'direct_pair', 'fixed', ?, ?, ?, ?, '', \
     'idle', 'private', ?, ?) ON CONFLICT (endpoint_a_kind, endpoint_a_id, endpoint_b_kind, \
     endpoint_b_id) WHERE placement_kind = 'direct_pair' DO NOTHING";
const SELECT_DIRECT_PAIR_FACT: &str = "SELECT id, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, \
     endpoint_b_id FROM conversations WHERE placement_kind = 'direct_pair' AND endpoint_a_kind = \
     ? AND endpoint_a_id = ? AND endpoint_b_kind = ? AND endpoint_b_id = ?";
const SELECT_DIRECT_PAIR_FACT_BY_ID: &str = "SELECT id, endpoint_a_kind, endpoint_a_id, \
     endpoint_b_kind, endpoint_b_id FROM conversations WHERE id = ? AND placement_kind = \
     'direct_pair'";
const SELECT_DIRECT_PAIR_ID: &str = "SELECT id FROM conversations WHERE placement_kind = \
     'direct_pair' AND endpoint_a_kind = ? AND endpoint_a_id = ? AND endpoint_b_kind = ? AND \
     endpoint_b_id = ?";

/// The copy is authored by the system: it is not the mentioning person speaking on
/// the line, and it names the message it came from so the two can never be
/// mistaken for independent facts.
const INSERT_MENTION_COPY: &str = "INSERT INTO messages (id, conversation_id, from_kind, content, \
     metadata, seq, created_at, cc_source_message_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)";
/// The author's own record that the mention left their conversation. Its
/// coordinates are the SOURCE's, because the reader is the author and their read
/// path is the message they can see — never the recipient's copy, which they cannot.
const INSERT_MENTION_NOTIFICATION: &str = "INSERT INTO notifications (id, kind, user_id, agent_id, \
     workspace_id, conversation_id, message_id, source_workspace_id, source_conversation_id, \
     source_message_id, payload, created_at) VALUES (?, 'mention_external', ?, ?, ?, ?, ?, ?, ?, \
     ?, ?, ?)";
const SELECT_USER_ID_BY_NAME: &str = "SELECT id FROM users WHERE name = ?";

/// Following is idempotent: a principal already following a cluster is not
/// followed again, and the insert reports which case happened so history is
/// backfilled exactly once.
const FOLLOW_CLUSTER: &str = "INSERT INTO thread_followers (thread_root_id, conversation_id, \
     principal_kind, principal_id, followed_at, source) VALUES (?, ?, ?, ?, ?, ?) ON \
     CONFLICT(thread_root_id, principal_kind, principal_id) DO NOTHING";
/// The cluster's recent history as of this reply: the last
/// `THREAD_BACKFILL_RECENT_REPLIES` earlier replies not written by the follower,
/// oldest first. The root is fetched separately because it is not one of its own
/// replies.
const SELECT_CLUSTER_HISTORY: &str = "SELECT id FROM (SELECT id, seq FROM messages WHERE \
     conversation_id = ? AND thread_root_id = ? AND seq < ? AND NOT (from_kind = 'agent' AND \
     from_agent_id = ?) ORDER BY seq DESC LIMIT ?) ORDER BY seq ASC";
/// A new follower is caught up with the root plus this many recent replies; older
/// replies stay readable through `message history`.
pub(crate) const THREAD_BACKFILL_RECENT_REPLIES: i64 = 10;
/// Delivery rows are idempotent per (conversation, agent, message), so a
/// re-followed cluster cannot queue the same message twice.
const ENSURE_INBOX_DELIVERY: &str = "INSERT INTO conversation_inboxes (conversation_id, agent_id, \
     message_id, delivered_at, delivery_kind, created_at) VALUES (?, ?, ?, NULL, ?, ?) ON \
     CONFLICT(conversation_id, agent_id, message_id) DO NOTHING";
/// Who a cluster's reply is for, as ONE set-shaped read. The follower rows for this
/// root bound the candidates, `JOIN agents` is the liveness conjunct, and the
/// placement predicate is correlated on each candidate's own id. Follower rows are
/// not authority over who may read a cluster — they are candidates participation
/// and liveness still have to accept — and they outlive both, so asking per
/// follower would put two point queries per row inside the writer's reservation
/// while the set itself grows with the cluster's history.
///
/// This is the SECOND text expressing one participation rule (the write gate holds
/// the point form), which is a real cost paid deliberately: a differential test
/// drives both across the placement arms, liveness, and lingering followers, and a
/// plan test keeps the candidate set bounded by the root rather than by the
/// installation.
macro_rules! cluster_audience_sql {
    () => {
        "SELECT tf.principal_id FROM thread_followers tf JOIN conversations c ON c.id = \
         tf.conversation_id JOIN agents a ON a.id = tf.principal_id WHERE tf.thread_root_id = ? \
         AND tf.principal_kind = 'agent' AND ((c.placement_kind = 'workspace' AND EXISTS (SELECT 1 \
         FROM conversation_agents ca WHERE ca.conversation_id = c.id AND ca.agent_id = a.id)) OR \
         (c.placement_kind = 'direct_pair' AND ((c.endpoint_a_kind = 'agent' AND c.endpoint_a_id = \
         a.id) OR (c.endpoint_b_kind = 'agent' AND c.endpoint_b_id = a.id)))) ORDER BY \
         tf.followed_at ASC, tf.principal_id ASC"
    };
}

const SELECT_CLUSTER_AUDIENCE: &str = cluster_audience_sql!();
/// The same statement, asked about its own plan, so the bound cannot be asserted
/// against a paraphrase.
#[cfg(test)]
const EXPLAIN_CLUSTER_AUDIENCE: &str = concat!("EXPLAIN QUERY PLAN ", cluster_audience_sql!());
const SELECT_ATTACHMENT_SOURCE: &str =
    "SELECT id, channel_id, filename, size, content_type FROM attachments WHERE id = ?";
const SELECT_AGENT_SESSION_SCOPE: &str = "SELECT session_scope FROM agents WHERE id = ?";

/// In-flight rows still count. A task system event's server-written actor is
/// authoritative; its assignee, mentions and sender-shaped content are not.
/// Ignore only the current agent's own known task events, without settling or
/// moving any inbox cursor. Unknown/missing actor metadata remains blocking.
macro_rules! freshness_relevant_inbox {
    () => {
        "i.delivered_at IS NULL AND i.failed_at IS NULL AND NOT EXISTS ( \
         SELECT 1 FROM messages m WHERE m.id=i.message_id AND m.from_kind='system' \
         AND CASE WHEN json_valid(m.metadata) THEN \
             json_extract(m.metadata,'$.subtype') IN \
                 ('task_create','task_edit','task_status','task_claim','task_unclaim') \
             AND json_type(m.metadata,'$.actor')='object' \
             AND json_extract(m.metadata,'$.actor.kind')='agent' \
             AND json_type(m.metadata,'$.actor.id')='text' \
             AND json_extract(m.metadata,'$.actor.id')=i.agent_id \
         ELSE 0 END)"
    };
}
const SELECT_UNDELIVERED_IN_CONVERSATION: &str = concat!(
    "SELECT 1 FROM conversation_inboxes i WHERE i.conversation_id=? AND i.agent_id=? AND ",
    freshness_relevant_inbox!(),
    " LIMIT 1"
);
const SELECT_UNDELIVERED_FOR_AGENT: &str = concat!(
    "SELECT 1 FROM conversation_inboxes i WHERE i.agent_id=? AND ",
    freshness_relevant_inbox!(),
    " LIMIT 1"
);

const RESERVE_NEXT_SEQ: &str =
    "UPDATE conversations SET latest_message_seq = latest_message_seq + 1 WHERE id = ?";
const SELECT_LATEST_SEQ: &str = "SELECT latest_message_seq FROM conversations WHERE id = ?";

pub(crate) const INSERT_MESSAGE: &str = "INSERT INTO messages (id, conversation_id, from_kind, from_agent_id, \
     from_user_id, content, metadata, seq, created_at, thread_root_id, author_id, author_name, \
     author_display_name, cc_source_message_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

/// Update-then-insert rather than upsert: the stream-head table's uniqueness is
/// two PARTIAL indexes, which cannot be named as a conflict target. The key
/// compares with `IS` because the main line's head is the row whose root is NULL.
const UPDATE_STREAM_HEAD: &str = "UPDATE conversation_stream_heads SET latest_seq = ? WHERE \
     conversation_id = ? AND thread_root_id IS ?";
const INSERT_STREAM_HEAD: &str = "INSERT INTO conversation_stream_heads (conversation_id, \
     thread_root_id, latest_seq) VALUES (?, ?, ?)";

/// Read the onboarding message's stored after-image back by id, so its shape is witnessed
/// from the row as persisted rather than from the values bound to the INSERT. Every column
/// this writer touches is read back, so the domain can bind the whole write — not just the
/// discriminating few — against the intent.
const READ_ONBOARDING_MESSAGE_BACK: &str = "SELECT id, conversation_id, from_kind, \
     from_agent_id, from_user_id, content, metadata, seq, created_at, thread_root_id, \
     author_id, author_name, author_display_name, cc_source_message_id \
     FROM messages WHERE id = ?";
/// Read the main line's stream-head ordinal (the row whose thread root is NULL), so the
/// domain can confirm the onboarding message is at the head of its stream.
const READ_MAIN_STREAM_HEAD_SEQ: &str = "SELECT latest_seq FROM conversation_stream_heads \
     WHERE conversation_id = ? AND thread_root_id IS NULL";

/// The cluster's list entry: the root's provenance and preview columns, alongside
/// the same authoritative `(reply_count, last_reply_*)` aggregate the badge uses
/// (`from_kind != 'system'`). The `agg` subquery is empty when no participant has
/// replied, so the JOIN yields no row and the entry is absent — the projection
/// never invents a placement for a cluster nobody has spoken in.
const THREAD_LIST_ENTRY_FOR_ROOT: &str = "SELECT \
     root.from_kind AS root_from_kind, root.from_agent_id AS root_from_agent_id, \
     root.from_user_id AS root_from_user_id, root.author_id AS root_author_id, \
     root.author_name AS root_author_name, root.author_display_name AS root_author_display_name, \
     root.content AS root_content, root.created_at AS root_created_at, root.seq AS root_seq, \
     agg.reply_count AS reply_count, lr.id AS last_reply_message_id, \
     lr.created_at AS last_reply_at \
     FROM (SELECT thread_root_id AS root_id, count(*) AS reply_count, max(seq) AS last_reply_seq \
     FROM messages WHERE conversation_id = ? AND thread_root_id = ? AND from_kind != 'system' \
     GROUP BY thread_root_id) agg \
     JOIN messages lr ON lr.conversation_id = ? AND lr.thread_root_id = agg.root_id \
     AND lr.seq = agg.last_reply_seq \
     JOIN messages root ON root.id = agg.root_id AND root.conversation_id = ?";

/// Distinct repliers of one cluster, most-recent-first. Grouping on the STABLE
/// `(from_kind, author_id)` snapshot — not the live FK — is what keeps two
/// hard-deleted repliers from collapsing into one NULL byline; the bare columns
/// follow SQLite's documented rule of coming from the `max(seq)` row of each group,
/// so an active replier resolves to its current identity and a deleted one to its
/// most recent send-time snapshot. System copies are excluded, matching the count.
const THREAD_REPLIERS_FOR_ROOT: &str = "SELECT from_kind, from_agent_id, from_user_id, author_id, \
     author_name, author_display_name, max(seq) AS last_seq FROM messages WHERE \
     conversation_id = ? AND thread_root_id = ? AND from_kind != 'system' \
     GROUP BY from_kind, author_id ORDER BY last_seq DESC";

const FROM_KIND_USER: &str = "user";
const FROM_KIND_AGENT: &str = "agent";
pub(crate) const FROM_KIND_SYSTEM: &str = "system";
const PRINCIPAL_KIND_HUMAN: &str = "human";
const PRINCIPAL_KIND_AGENT: &str = "agent";
const FOLLOWER_SOURCE_REPLY: &str = "reply";
const FOLLOWER_SOURCE_TARGET_AUTHOR: &str = "target_author";
const FOLLOWER_SOURCE_MENTION: &str = "mention";
/// Historical context a follower is catching up on, as opposed to a message that
/// arrived while it was already following.
const DELIVERY_KIND_OBSERVE: &str = "observe";
/// A message that arrived for someone now: it is owed an answer, not context.
const DELIVERY_KIND_TURN: &str = "turn";
const PLACEMENT_DIRECT_PAIR: &str = "direct_pair";
const ENDPOINT_KIND_HUMAN: &str = "human";
const ENDPOINT_KIND_AGENT: &str = "agent";

/// The host capabilities this store uses unless a caller injects others.
#[derive(Clone, Copy)]
pub struct HostEnvironment;

impl SendClock for HostEnvironment {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        unix_ms_since_epoch(SystemTime::now())
    }
}

impl SendEntropy for HostEnvironment {
    fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        getrandom::fill(buffer).map_err(EnvironmentUnavailable::from_source)
    }
}

/// The conversion the clock performs, separated from reading the clock so the unit
/// it answers in can be asserted without depending on what time it is.
fn unix_ms_since_epoch(moment: SystemTime) -> Result<i64, EnvironmentUnavailable> {
    let since_epoch = moment
        .duration_since(UNIX_EPOCH)
        .map_err(EnvironmentUnavailable::from_source)?;
    i64::try_from(since_epoch.as_millis()).map_err(EnvironmentUnavailable::from_source)
}

#[derive(Clone)]
pub struct MessagingStore<E = HostEnvironment> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}

impl<E> MessagingStore<E> {
    /// The shared connection pool, for a sibling read-model adapter in another
    /// crate module to derive its own shared-pool clone (never a second
    /// connection authority). Crate-visible: only in-crate adapters may take it.
    pub(crate) fn pool(&self) -> &crate::foundation::database::SqliteDatabase {
        &self.pool
    }
}

/// The post-commit onboarding inbox-ensure authority (built by
/// [`MessagingStore::onboarding_inbox_ensure`]). It holds the store's shared pool and clock
/// and implements [`OnboardingInboxEnsure`] by composing three EXISTING statements — the
/// message-in-conversation read, the agent-participation read, and the idempotent delivery
/// insert — in one transaction, so it never parallel-copies the send path's SQL.
#[derive(Clone)]
pub struct SqliteOnboardingInboxEnsure<E = HostEnvironment> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}

/// Message-generic form of the same idempotent inbox authority. It shares the
/// store pool, clock, witness reads, and `ENSURE_INBOX_DELIVERY` statement; only
/// the onboarding-only subtype witness is intentionally absent.
#[derive(Clone)]
pub struct SqliteCommittedMessageInboxEnsure<E = HostEnvironment> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}

/// Why an onboarding inbox-ensure could not be made. Wrapped in an [`EffectFailure`] so the
/// create route's best-effort leg logs it and moves on; the row is never written when the
/// relationship the caller named is not a committed fact.
#[derive(Debug, Error)]
pub enum OnboardingInboxEnsureError {
    /// A witness the ensure depends on did not hold: the message is not in the named
    /// conversation, the message is not an onboarding system message, or the agent is not a
    /// current participant. Reported rather than written — and named by WHICH fact failed, so
    /// the error chain never lies — so a liar caller or a concurrent membership change cannot
    /// manufacture a cross-conversation inbox debt.
    #[error("onboarding delivery witness did not hold")]
    WitnessMismatch(#[from] DeliveryWitnessMismatch),
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("the clock this write stamps with could not answer")]
    Environment(#[from] EnvironmentUnavailable),
}

/// The three distinct facts the onboarding inbox-ensure witnesses before it writes, named
/// apart so a fault reports the ONE that actually failed rather than fusing them. Each is a
/// committed-fact check, not a caller assertion: the message's home conversation, the message's
/// onboarding shape, and the agent's current participation.
#[derive(Debug, Error)]
pub enum DeliveryWitnessMismatch {
    /// The message does not belong to the conversation the route named.
    #[error("the message does not belong to the named conversation")]
    Message,
    /// The message is in the conversation but is not a genuine onboarding system message
    /// (wrong `from_kind`, or metadata that is not exactly the onboarding subtype).
    #[error("the message is not an onboarding system message: {reason}")]
    OnboardingShape { reason: &'static str },
    /// The message and its shape hold, but the agent is not a current participant of the
    /// conversation (no workspace roster row and no direct-pair endpoint).
    #[error("the agent is not a current participant of the conversation")]
    Participation,
}

impl<E> OnboardingInboxEnsure for SqliteOnboardingInboxEnsure<E>
where
    E: SendClock + Clone + Send + Sync + 'static,
{
    async fn ensure(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
        onboarding_message: &MessageId,
    ) -> Result<InboxEnsureOutcome, EffectFailure> {
        self.ensure_inner(conversation, agent, onboarding_message)
            .await
            .map_err(EffectFailure::from_source)
    }
}

impl<E> LiveMessageInboxEnsure for SqliteCommittedMessageInboxEnsure<E>
where
    E: SendClock + Clone + Send + Sync + 'static,
{
    async fn ensure_for_live_recipient(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
        message: &MessageId,
    ) -> Result<InboxEnsureOutcome, EffectFailure> {
        ensure_inbox_delivery(
            &self.pool,
            &self.environment,
            conversation,
            agent,
            message,
            InboxRecipientAuthority::LiveParticipation,
        )
        .await
        .map_err(EffectFailure::from_source)
    }
}

impl<E> SealedMessageInboxEnsure for SqliteCommittedMessageInboxEnsure<E>
where
    E: SendClock + Clone + Send + Sync + 'static,
{
    async fn ensure_for_sealed_recipient(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
        message: &MessageId,
    ) -> Result<InboxEnsureOutcome, EffectFailure> {
        ensure_inbox_delivery(
            &self.pool,
            &self.environment,
            conversation,
            agent,
            message,
            InboxRecipientAuthority::SealedByCommit,
        )
        .await
        .map_err(EffectFailure::from_source)
    }
}

impl<E: SendClock> SqliteOnboardingInboxEnsure<E> {
    async fn ensure_inner(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
        onboarding_message: &MessageId,
    ) -> Result<InboxEnsureOutcome, OnboardingInboxEnsureError> {
        ensure_inbox_delivery(
            &self.pool,
            &self.environment,
            conversation,
            agent,
            onboarding_message,
            InboxRecipientAuthority::Onboarding,
        )
        .await
    }
}

#[derive(Clone, Copy)]
enum InboxRecipientAuthority {
    Onboarding,
    LiveParticipation,
    SealedByCommit,
}

async fn ensure_inbox_delivery<E: SendClock>(
    pool: &crate::foundation::database::SqliteDatabase,
    environment: &E,
    conversation: &ConversationId,
    agent: &AgentId,
    message_id: &MessageId,
    authority: InboxRecipientAuthority,
) -> Result<InboxEnsureOutcome, OnboardingInboxEnsureError> {
    // Stamp with the store's own clock, ahead of opening the transaction.
    let now = environment.now_unix_ms()?;
    // Reserve the writer BEFORE reading the message/recipient witnesses. Waiting
    // here permits a competing writer to finish; upgrading a stale read does not.
    let mut tx = pool.begin_with(ROOT_WRITE_BEGIN).await?;

    // Witness 1 — the message belongs to THIS conversation AND is a genuine onboarding
    // system message. One read returns the two discriminating columns; the shape is judged
    // by the domain's ONE recognizer over the ONE metadata decode, so this narrow door
    // cannot be tricked into enqueuing an ordinary same-conversation message a participant
    // happened to send. A row absent from the conversation and a row of the wrong shape are
    // named apart in the fault.
    let message = sqlx::query(SELECT_ONBOARDING_MESSAGE_SHAPE_IN_CONVERSATION)
        .bind(conversation.as_str())
        .bind(message_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
    let Some(message) = message else {
        return Err(DeliveryWitnessMismatch::Message.into());
    };
    if matches!(authority, InboxRecipientAuthority::Onboarding) {
        let from_kind: String = message.try_get("from_kind")?;
        let metadata: Option<String> = message.try_get("metadata")?;
        let digest = decode_onboarding_metadata_digest(metadata.as_deref());
        messaging::agent_onboarding::recognize_onboarding_shape(&from_kind, &digest).map_err(
            |mismatch| DeliveryWitnessMismatch::OnboardingShape {
                reason: mismatch.reason(),
            },
        )?;
    }

    // Witness 2 — the agent is a current participant of that conversation, via the ONE
    // canonical participation predicate the send path's `is_agent_participant` uses
    // (roster row for a workspace placement, or a direct-pair endpoint).
    if !matches!(authority, InboxRecipientAuthority::SealedByCommit) {
        let participates: Option<i64> = sqlx::query_scalar(SELECT_AGENT_PARTICIPATES)
            .bind(conversation.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        if participates.is_none() {
            return Err(DeliveryWitnessMismatch::Participation.into());
        }
    }

    if !conversation_allows_delivery(&mut tx, conversation).await? {
        tx.commit().await?;
        return Ok(InboxEnsureOutcome::SkippedArchived);
    }

    // The witnesses hold: enqueue the durable turn. The SAME idempotent writer the send
    // fan-out uses — a re-run (echo) conflicts on the unique delivery index and writes
    // nothing new.
    sqlx::query(ENSURE_INBOX_DELIVERY)
        .bind(conversation.as_str())
        .bind(agent.as_str())
        .bind(message_id.as_str())
        .bind(DELIVERY_KIND_TURN)
        .bind(now)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(InboxEnsureOutcome::Ready)
}

/// Startup and driver failures, surfaced only to the composition root that opens
/// the store. The port below never exposes it.
#[derive(Debug, Error)]
pub enum MessagingStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("attachment snapshot could not be serialized")]
    Serialization(#[from] serde_json::Error),
    /// A stored ordinal outside the domain's range. The column is the store's own,
    /// so this is a broken database rather than a caller's mistake.
    #[error("stored message ordinal is outside the range the domain defines")]
    Ordinal(#[from] InvalidSeq),
    /// A direct pair whose endpoint columns are absent or unknown contradicts the
    /// schema's own constraints, so there is no participation question to answer.
    #[error("conversation row contradicts the placement constraints of its schema")]
    MalformedPlacement,
    /// A stored column that should name something names nothing. Unrelated to
    /// placement, and named apart from it so a reader is not misdirected.
    #[error("stored column {field} does not name a usable identity")]
    MalformedIdentity { field: &'static str },
    /// A stored runtime spelling outside the closed set the domain accepts. The
    /// column is the store's own, so a value it never writes is a broken database,
    /// reported with the offending spelling rather than defaulted to a runtime.
    #[error("stored runtime {value:?} is outside the set the domain accepts")]
    MalformedRuntime { value: String },
    /// A host capability the write depends on could not answer. Kept as a source
    /// rather than a marker: "no identity could be minted" is only actionable
    /// with the reason attached.
    #[error("a host capability this write depends on could not answer")]
    Environment(#[from] EnvironmentUnavailable),
    /// A committed reply projected no cluster entry, though the reply it just wrote is
    /// itself a non-system participant of the cluster and must be visible inside the
    /// same transaction. Its absence is a broken invariant, not a caller's mistake.
    #[error("a committed reply projected no cluster entry")]
    MissingClusterProjection,
}

fn infrastructure(error: MessagingStoreError) -> SendMessageError {
    PersistenceFailure::from_source(error).into()
}

fn driver(error: sqlx::Error) -> PersistenceFailure {
    PersistenceFailure::from_source(MessagingStoreError::Sql(error))
}

impl MessagingStore<HostEnvironment> {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self {
            pool,
            environment: HostEnvironment,
        }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, MessagingStoreError> {
        Self::open_with(path, HostEnvironment).await
    }
}

impl<E> MessagingStore<E> {
    pub fn target_address<O, R>(&self, opened: O, refusals: R) -> SqliteTargetAddress<E, O, R>
    where
        E: Clone,
    {
        SqliteTargetAddress::new(self.clone(), opened, refusals)
    }

    /// Preflight the schedule consumer's `manage_schedules` capability against
    /// the resolved conversation placement. The create writer repeats this under
    /// its reserved lock; this read exists only to preserve route refusal order.
    pub async fn may_manage_schedules(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
    ) -> Result<bool, PersistenceFailure> {
        let value: Option<i64> = sqlx::query_scalar(concat!(
            "SELECT CASE WHEN c.placement_kind='direct_pair' THEN 1 \
             WHEN c.placement_kind='workspace' AND EXISTS(SELECT 1 FROM workspace_members wm \
             WHERE wm.workspace_id=c.workspace_id AND wm.agent_id=?) AND EXISTS(SELECT 1 FROM \
             workspaces w WHERE w.id=c.workspace_id AND ",
            crate::foundation::capability::capability_predicate!("manage_schedules"),
            ") THEN 1 ELSE 0 END FROM conversations c WHERE c.id=?"
        ))
        .bind(agent.as_str())
        .bind(agent.as_str())
        .bind(agent.as_str())
        .bind(conversation.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(driver)?;
        Ok(value == Some(1))
    }
    /// Opens an existing product database with injected host capabilities. A
    /// missing file is a failure rather than an empty database.
    pub async fn open_with(
        path: impl AsRef<Path>,
        environment: E,
    ) -> Result<Self, MessagingStoreError> {
        Self::open_with_busy_timeout(path, environment, BUSY_TIMEOUT).await
    }

    /// The readiness write probe for the database THIS store writes to.
    ///
    /// The probe is handed the store's own handle, not its path and not a copy of its
    /// configuration: readiness has to answer for the writer the process actually
    /// uses, and a probe holding a handle of its own would report a writable database
    /// through a connection nothing else writes on.
    pub fn write_probe(&self) -> crate::foundation::write_probe::SqliteWriteProbe {
        crate::foundation::write_probe::SqliteWriteProbe::sharing(self.pool.clone())
    }

    /// The post-commit onboarding inbox-ensure authority for the database THIS store
    /// writes to. Like every other narrow door it shares the store's own pool handle (no
    /// second connection identity), and carries the store's clock so the ensured row's
    /// `created_at` comes from the same authority every other write here uses.
    pub fn onboarding_inbox_ensure(&self) -> SqliteOnboardingInboxEnsure<E>
    where
        E: Clone,
    {
        SqliteOnboardingInboxEnsure {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }

    pub fn committed_message_inbox_ensure(&self) -> SqliteCommittedMessageInboxEnsure<E>
    where
        E: Clone,
    {
        SqliteCommittedMessageInboxEnsure {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }

    pub(crate) async fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        environment: E,
        busy_timeout: Duration,
    ) -> Result<Self, MessagingStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), busy_timeout).await?;

        Ok(Self { pool, environment })
    }

    /// Reserves the single writer before the first statement, so contention
    /// surfaces before any of the unit's reads are trusted.
    async fn begin_root_write(&self) -> Result<Transaction<'_, Sqlite>, sqlx::Error> {
        self.pool.begin_with(ROOT_WRITE_BEGIN).await
    }

    /// The cluster's list entry as a **read-side** projection over committed rows —
    /// for a client placing a thread it has not observed the reply event for (a late
    /// join, a refetch, a reconnect) — or `None` when the root has no non-system reply
    /// and there is nothing to place.
    ///
    /// This is NOT the authority for a reply's own frame: that entry is projected
    /// inside the reply's atomic unit and carried on [`ThreadReplyFact`], so a rename
    /// or a newer reply after the commit cannot change what was published. Both go
    /// through the one [`project_list_entry`], so this cold read and the in-transaction
    /// projection cannot diverge (proved by the differential gate).
    pub async fn thread_list_entry_for_root(
        &self,
        conversation: &ConversationId,
        root: &ThreadRootId,
    ) -> Result<Option<ThreadListEntry>, PersistenceFailure> {
        let mut tx = self.pool.begin().await.map_err(driver)?;
        project_list_entry(&mut tx, conversation, root).await
    }
}

impl<E: SendEntropy + Send + Sync> MessagingStore<E> {
    pub async fn open_direct_pair<O>(
        &self,
        opened: &O,
        participants: DirectPairParticipants,
        now_unix_ms: i64,
    ) -> Result<ConversationId, OpenDirectPairError<PersistenceFailure, O::Error>>
    where
        O: DirectPairOpenedPort,
    {
        let mut mint = AutocommitDirectPairMint { store: self };
        open_direct_pair(&mut mint, opened, participants, now_unix_ms).await
    }
}

struct AutocommitDirectPairMint<'store, E> {
    store: &'store MessagingStore<E>,
}

impl<E: SendEntropy + Send + Sync> DirectPairMint for AutocommitDirectPairMint<'_, E> {
    type Error = PersistenceFailure;

    async fn mint_direct_pair(
        &mut self,
        participants: &DirectPairParticipants,
        now_unix_ms: i64,
    ) -> Result<DirectPairMinted, Self::Error> {
        // Entropy is host work, not product authority. Sample it before the
        // writer reservation; the transaction below still derives the winner.
        let minted = mint_conversation_id(&self.store.environment).map_err(|source| {
            PersistenceFailure::from_source(MessagingStoreError::Environment(source))
        })?;
        let mut tx = self
            .store
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(driver)?;
        let committed = mint_direct_pair_on(&mut tx, minted, participants, now_unix_ms)
            .await
            .map_err(PersistenceFailure::from_source)?;
        tx.commit().await.map_err(driver)?;
        Ok(committed)
    }
}

fn principal_kind(principal: &PrincipalIdentity) -> &'static str {
    match principal.kind() {
        PrincipalKind::Agent => ENDPOINT_KIND_AGENT,
        PrincipalKind::Human => ENDPOINT_KIND_HUMAN,
    }
}

fn decode_selected_direct_pair(
    row: &SqliteRow,
) -> Result<(ConversationId, DirectPairParticipants), MessagingStoreError> {
    let conversation: String = row.try_get("id")?;
    let conversation = ConversationId::parse(conversation).map_err(|_| {
        MessagingStoreError::MalformedIdentity {
            field: "conversations.id",
        }
    })?;
    let first = decode_direct_pair_endpoint(row, "endpoint_a_kind", "endpoint_a_id")?;
    let second = decode_direct_pair_endpoint(row, "endpoint_b_kind", "endpoint_b_id")?;
    let participants = DirectPairParticipants::between(first, second)
        .map_err(|_| MessagingStoreError::MalformedPlacement)?;
    Ok((conversation, participants))
}

fn decode_direct_pair_endpoint(
    row: &SqliteRow,
    kind_field: &'static str,
    id_field: &'static str,
) -> Result<PrincipalIdentity, MessagingStoreError> {
    let kind: Option<String> = row.try_get(kind_field)?;
    let id: Option<String> = row.try_get(id_field)?;
    match (kind.as_deref(), id) {
        (Some(ENDPOINT_KIND_AGENT), Some(raw)) => Ok(PrincipalIdentity::Agent(
            AgentId::parse(raw)
                .map_err(|_| MessagingStoreError::MalformedIdentity { field: id_field })?,
        )),
        (Some(ENDPOINT_KIND_HUMAN), Some(raw)) => Ok(PrincipalIdentity::Human(
            UserId::parse(raw)
                .map_err(|_| MessagingStoreError::MalformedIdentity { field: id_field })?,
        )),
        _ => Err(MessagingStoreError::MalformedIdentity { field: kind_field }),
    }
}

/// The ordinal allocation invariant, as one tx-composable writer: advance the
/// conversation's global counter and read back what it reserved. Every writer that
/// appends a message shares it — the send unit of work, a mention copy, and
/// provisioning's onboarding message — so `(conversation, ordinal)` has exactly one
/// allocator and cannot fork into a second grammar. A value the domain rejects is a
/// broken database, reported with the value that broke it rather than defaulted.
pub(crate) async fn reserve_next_seq_on(
    conn: &mut SqliteConnection,
    conversation: &ConversationId,
) -> Result<MessageSeq, ReserveSeqFault> {
    sqlx::query(RESERVE_NEXT_SEQ)
        .bind(conversation.as_str())
        .execute(&mut *conn)
        .await
        .map_err(ReserveSeqFault::Driver)?;
    let reserved: i64 = sqlx::query_scalar(SELECT_LATEST_SEQ)
        .bind(conversation.as_str())
        .fetch_one(&mut *conn)
        .await
        .map_err(ReserveSeqFault::Driver)?;
    MessageSeq::try_from(reserved).map_err(ReserveSeqFault::Ordinal)
}

/// A reserve that could not complete. The two failures are kept apart because they
/// answer to different owners: a driver error is infrastructure, an out-of-range
/// ordinal is a broken invariant in the store's own column.
pub(crate) enum ReserveSeqFault {
    Driver(sqlx::Error),
    Ordinal(InvalidSeq),
}

/// The stream-head advance invariant, as one tx-composable writer. Update-then-insert
/// rather than upsert because the head table's uniqueness is two PARTIAL indexes,
/// which cannot be named as a conflict target; the key compares with `IS` because the
/// main line's head is the row whose root is NULL. Shared by every append path, so no
/// second head writer exists to disagree.
pub(crate) async fn bump_stream_head_on(
    conn: &mut SqliteConnection,
    conversation: &ConversationId,
    thread_root: Option<&ThreadRootId>,
    seq: MessageSeq,
) -> Result<(), sqlx::Error> {
    let root = thread_root.map(ThreadRootId::as_str);
    let updated = sqlx::query(UPDATE_STREAM_HEAD)
        .bind(seq.get())
        .bind(conversation.as_str())
        .bind(root)
        .execute(&mut *conn)
        .await?
        .rows_affected();

    if updated == 0 {
        sqlx::query(INSERT_STREAM_HEAD)
            .bind(conversation.as_str())
            .bind(root)
            .bind(seq.get())
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Append the onboarding system message as one complete invariant cluster: reserve
/// the conversation's next ordinal, insert the row in the fixed system shape, and
/// advance the main-line stream head — the same three facts every send writes.
///
/// This is provisioning's message writer, NOT a generic insert seam. The intent
/// decides only WHERE the message lands and WHAT it says; the kind is always
/// `system`, there is no author and no thread root, and the metadata carries only the
/// onboarding subtype. A caller cannot reach through it to write an agent- or
/// user-authored row, or to stamp a different subtype.
pub(crate) async fn append_onboarding_system_message_on(
    conn: &mut SqliteConnection,
    intent: &messaging::agent_onboarding::OnboardingSystemMessage,
) -> Result<messaging::agent_onboarding::OnboardingWriteWitness, PersistenceFailure> {
    let seq = reserve_next_seq_on(conn, intent.conversation())
        .await
        .map_err(|fault| match fault {
            ReserveSeqFault::Driver(error) => driver(error),
            ReserveSeqFault::Ordinal(error) => {
                PersistenceFailure::from_source(MessagingStoreError::Ordinal(error))
            }
        })?;
    let metadata = serde_json::to_string(&serde_json::json!({ "subtype": intent.subtype() }))
        .map_err(|error| {
            PersistenceFailure::from_source(MessagingStoreError::Serialization(error))
        })?;

    sqlx::query(INSERT_MESSAGE)
        .bind(intent.id().as_str())
        .bind(intent.conversation().as_str())
        .bind(FROM_KIND_SYSTEM)
        .bind(None::<&str>)
        .bind(None::<&str>)
        .bind(intent.content())
        .bind(metadata)
        .bind(seq.get())
        .bind(intent.created_at_ms())
        .bind(None::<&str>)
        .bind(None::<&str>)
        .bind(None::<&str>)
        .bind(None::<&str>)
        .bind(None::<&str>)
        .execute(&mut *conn)
        .await
        .map_err(driver)?;

    bump_stream_head_on(conn, intent.conversation(), None, seq)
        .await
        .map_err(driver)?;

    // Witness the write from the DB's own after-image — the row as stored and the main-line
    // stream head as bumped — rather than re-asserting what we bound. The metadata's JSON
    // encoding is this layer's concern (it serialized it on the way in), so it decodes the
    // subtype here and hands the domain the discriminator, not the blob. The domain
    // (`OnboardingSystemMessage::seal_written`) re-derives the message's shape and head
    // position from these raw facts; this adapter mints no verdict.
    let message_row = sqlx::query(READ_ONBOARDING_MESSAGE_BACK)
        .bind(intent.id().as_str())
        .fetch_one(&mut *conn)
        .await
        .map_err(driver)?;
    let head_latest_seq = sqlx::query_scalar::<_, i64>(READ_MAIN_STREAM_HEAD_SEQ)
        .bind(intent.conversation().as_str())
        .fetch_one(&mut *conn)
        .await
        .map_err(driver)?;

    // The metadata's JSON encoding is this layer's concern (it serialized it on the way in),
    // so it decodes the blob into a `{subtype, has_extra_keys}` digest and hands the domain
    // the discriminator, not the blob — the domain has no serde. This is the ONE decode, shared
    // with the post-commit inbox-ensure's shape witness, so the JSON grammar is never spelled twice.
    let digest = decode_onboarding_metadata_digest(
        message_row
            .try_get::<Option<String>, _>("metadata")
            .map_err(driver)?
            .as_deref(),
    );

    Ok(messaging::agent_onboarding::OnboardingWriteWitness {
        id: message_row.try_get::<String, _>("id").map_err(driver)?,
        conversation: message_row
            .try_get::<String, _>("conversation_id")
            .map_err(driver)?,
        content: message_row
            .try_get::<String, _>("content")
            .map_err(driver)?,
        created_at_ms: message_row
            .try_get::<i64, _>("created_at")
            .map_err(driver)?,
        from_kind: message_row
            .try_get::<String, _>("from_kind")
            .map_err(driver)?,
        from_agent_id: message_row
            .try_get::<Option<String>, _>("from_agent_id")
            .map_err(driver)?,
        from_user_id: message_row
            .try_get::<Option<String>, _>("from_user_id")
            .map_err(driver)?,
        author_id: message_row
            .try_get::<Option<String>, _>("author_id")
            .map_err(driver)?,
        author_name: message_row
            .try_get::<Option<String>, _>("author_name")
            .map_err(driver)?,
        author_display_name: message_row
            .try_get::<Option<String>, _>("author_display_name")
            .map_err(driver)?,
        thread_root_id: message_row
            .try_get::<Option<String>, _>("thread_root_id")
            .map_err(driver)?,
        cc_source_message_id: message_row
            .try_get::<Option<String>, _>("cc_source_message_id")
            .map_err(driver)?,
        metadata: digest,
        seq: message_row.try_get::<i64, _>("seq").map_err(driver)?,
        head_latest_seq,
    })
}

/// Decode a stored message's metadata blob into the digest the onboarding domain judges — the
/// `subtype` under that key, and whether the object carried ANY other key. This is the ONE
/// metadata decode, used by both the write-back seal and the post-commit inbox-ensure's shape
/// witness, so the JSON grammar the domain reasons about is never spelled twice. A non-object,
/// absent, or unparseable blob carries no subtype (the missing subtype already refuses).
fn decode_onboarding_metadata_digest(
    raw: Option<&str>,
) -> messaging::agent_onboarding::OnboardingMetadataDigest {
    match raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok()) {
        Some(serde_json::Value::Object(map)) => {
            let subtype = map
                .get("subtype")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let has_extra_keys = map.keys().any(|key| key != "subtype");
            messaging::agent_onboarding::OnboardingMetadataDigest {
                subtype,
                has_extra_keys,
            }
        }
        _ => messaging::agent_onboarding::OnboardingMetadataDigest {
            subtype: None,
            has_extra_keys: false,
        },
    }
}

/// Open a direct pair on a caller-provided connection, composing the get-or-create mint
/// and the identity check WITHOUT opening or committing a transaction of its own. A caller
/// can therefore make it one step of a larger atomic unit of work — agent provisioning
/// writes the activation flip, this direct line, and the onboarding message under a SINGLE
/// transaction, so a dependent-write failure rolls the whole thing back. The candidate id
/// seeds the row only if this open wins the get-or-create; `opened` is announced only on a
/// fresh mint, exactly as [`open_direct_pair`] specifies, so `pair_created` is observable to
/// the caller as whether the recorder captured a fact.
pub(crate) async fn open_direct_pair_on<O: DirectPairOpenedPort>(
    conn: &mut SqliteConnection,
    candidate: ConversationId,
    opened: &O,
    participants: DirectPairParticipants,
    now_unix_ms: i64,
) -> Result<ConversationId, OpenDirectPairError<PersistenceFailure, O::Error>> {
    let mut mint = ConnectionDirectPairMint { conn, candidate };
    open_direct_pair(&mut mint, opened, participants, now_unix_ms).await
}

/// A [`DirectPairMint`] that mints on a borrowed connection using a candidate id the caller
/// already holds, so the get-or-create runs inside the caller's transaction rather than one
/// of its own. Unlike [`AutocommitDirectPairMint`], it never opens or commits — that is the
/// whole point: it composes into a wider unit of work.
struct ConnectionDirectPairMint<'connection> {
    conn: &'connection mut SqliteConnection,
    candidate: ConversationId,
}

impl DirectPairMint for ConnectionDirectPairMint<'_> {
    type Error = PersistenceFailure;

    async fn mint_direct_pair(
        &mut self,
        participants: &DirectPairParticipants,
        now_unix_ms: i64,
    ) -> Result<DirectPairMinted, Self::Error> {
        mint_direct_pair_on(self.conn, self.candidate.clone(), participants, now_unix_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

async fn mint_direct_pair_on(
    conn: &mut SqliteConnection,
    minted: ConversationId,
    participants: &DirectPairParticipants,
    now_unix_ms: i64,
) -> Result<DirectPairMinted, MessagingStoreError> {
    let first = participants.first();
    let second = participants.second();
    let created = sqlx::query(OPEN_DIRECT_PAIR)
        .bind(minted.as_str())
        .bind(principal_kind(first))
        .bind(first.as_str())
        .bind(principal_kind(second))
        .bind(second.as_str())
        .bind(now_unix_ms)
        .bind(now_unix_ms)
        .execute(&mut *conn)
        .await?
        .rows_affected()
        == 1;

    // A winner is read back by the minted identity, so an AFTER trigger cannot
    // silently rewrite endpoints. A loser resolves the row that actually won by
    // its canonical durable endpoint key. Both branches decode the row itself.
    let selected = if created {
        sqlx::query(SELECT_DIRECT_PAIR_FACT_BY_ID)
            .bind(minted.as_str())
            .fetch_optional(&mut *conn)
            .await?
    } else {
        sqlx::query(SELECT_DIRECT_PAIR_FACT)
            .bind(principal_kind(first))
            .bind(first.as_str())
            .bind(principal_kind(second))
            .bind(second.as_str())
            .fetch_optional(&mut *conn)
            .await?
    }
    .ok_or(MessagingStoreError::MalformedIdentity {
        field: "conversations.direct_pair",
    })?;
    let (conversation, selected_participants) = decode_selected_direct_pair(&selected)?;
    if &selected_participants != participants {
        return Err(MessagingStoreError::MalformedPlacement);
    }
    Ok(DirectPairMinted::committed(
        conversation,
        selected_participants,
        created,
    ))
}

/// Projects the cluster's list entry from the committed rows visible on `conn`.
///
/// It reads the authoritative `(reply_count, last_reply_*)` aggregate the badge uses
/// and resolves every author — the root's and each distinct replier's — through the
/// adapter's one author authority ([`resolve_authors`]) in a SINGLE batch, so a whole
/// entry costs the same two set-shaped author reads a page row does. A replier with no
/// stable identity (a legacy pre-snapshot deleted actor) renders nothing and does not
/// aggregate, so it is dropped from both the shown set and the total; a deleted actor
/// keeps its snapshot identity and counts. The two structural reads share `conn`, so
/// the count and the repliers cannot be read from different snapshots.
///
/// Shared by the send unit of work (passing its own root transaction, so the entry it
/// folds into the reply fact is the committed one) and the read-side consumer above
/// (passing a fresh read transaction). One projection, so the two cannot disagree.
async fn project_list_entry(
    conn: &mut SqliteConnection,
    conversation: &ConversationId,
    root: &ThreadRootId,
) -> Result<Option<ThreadListEntry>, PersistenceFailure> {
    let Some(row) = sqlx::query(THREAD_LIST_ENTRY_FOR_ROOT)
        .bind(conversation.as_str())
        .bind(root.as_str())
        .bind(conversation.as_str())
        .bind(conversation.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(driver)?
    else {
        return Ok(None);
    };

    let root_from_kind: String = row.try_get("root_from_kind").map_err(driver)?;
    let root_content: String = row.try_get("root_content").map_err(driver)?;
    let root_created_at: i64 = row.try_get("root_created_at").map_err(driver)?;
    let root_seq_raw: i64 = row.try_get("root_seq").map_err(driver)?;
    let reply_count: i64 = row.try_get("reply_count").map_err(driver)?;
    let last_reply_message_id: String = row.try_get("last_reply_message_id").map_err(driver)?;
    let last_reply_at: i64 = row.try_get("last_reply_at").map_err(driver)?;

    // The root's author leads the batch; each distinct replier follows in
    // most-recent-first order, so one resolver call answers the whole entry.
    let mut evidence = vec![AuthorEvidence {
        from_kind: from_kind_of(&root_from_kind)?,
        from_agent_id: row.try_get("root_from_agent_id").map_err(driver)?,
        from_user_id: row.try_get("root_from_user_id").map_err(driver)?,
        author_id: row.try_get("root_author_id").map_err(driver)?,
        author_name: row.try_get("root_author_name").map_err(driver)?,
        author_display_name: row.try_get("root_author_display_name").map_err(driver)?,
    }];

    let replier_rows = sqlx::query(THREAD_REPLIERS_FOR_ROOT)
        .bind(conversation.as_str())
        .bind(root.as_str())
        .fetch_all(&mut *conn)
        .await
        .map_err(driver)?;
    for replier in &replier_rows {
        let from_kind: String = replier.try_get("from_kind").map_err(driver)?;
        evidence.push(AuthorEvidence {
            from_kind: from_kind_of(&from_kind)?,
            from_agent_id: replier.try_get("from_agent_id").map_err(driver)?,
            from_user_id: replier.try_get("from_user_id").map_err(driver)?,
            author_id: replier.try_get("author_id").map_err(driver)?,
            author_name: replier.try_get("author_name").map_err(driver)?,
            author_display_name: replier.try_get("author_display_name").map_err(driver)?,
        });
    }

    let mut resolved = resolve_authors(&mut *conn, &evidence).await?.into_iter();
    // A system root has no author; the resolver already answers `None` for it.
    let root_author = resolved.next().flatten();

    let mut repliers = Vec::new();
    let mut replier_total: i64 = 0;
    for author in resolved.flatten() {
        if author.stable_identity().is_none() {
            continue;
        }
        replier_total += 1;
        if repliers.len() < 3 {
            repliers.push(author);
        }
    }

    Ok(Some(ThreadListEntry::committed(
        root.clone(),
        root_author,
        &root_content,
        root_created_at,
        MessageSeq::try_from(root_seq_raw).map_err(|error| {
            PersistenceFailure::from_source(MessagingStoreError::Ordinal(error))
        })?,
        reply_count,
        last_reply_at,
        MessageId::parse(last_reply_message_id).map_err(|_| malformed("messages.id"))?,
        repliers,
        replier_total,
    )))
}

mod send;
use send::{from_kind_of, malformed};

#[cfg(test)]
mod tests;

mod profile_message_counts;
