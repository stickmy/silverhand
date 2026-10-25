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
        let value: Option<i64> = sqlx::query_scalar(
            "SELECT CASE WHEN c.placement_kind='direct_pair' THEN 1 \
             WHEN c.placement_kind='workspace' AND EXISTS(SELECT 1 FROM workspace_members wm \
             WHERE wm.workspace_id=c.workspace_id AND wm.agent_id=?) AND (EXISTS(SELECT 1 FROM \
             workspaces w WHERE w.id=c.workspace_id AND w.owner_agent_id=?) OR EXISTS(SELECT 1 \
             FROM workspace_agent_capabilities cap WHERE cap.workspace_id=c.workspace_id AND \
             cap.agent_id=? AND cap.capability='manage_schedules')) THEN 1 ELSE 0 END \
             FROM conversations c WHERE c.id=?",
        )
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

impl<E: SendClock + SendEntropy + Send + Sync> MessagingStore<E> {
    #[tracing::instrument(skip(self, command), fields(
        target_conversation = command.target_conversation().as_str(),
        content_bytes = command.content().byte_length(),
    ))]
    async fn send_message_transaction(
        &self,
        command: SendMessage,
    ) -> Result<SendMessageOutcome, SendMessageError> {
        // The environment is asked before a transaction exists, so a host that
        // cannot answer costs nothing: there is no writer to reserve and no
        // statement to roll back.
        let created_at_ms = self.environment.now_unix_ms()?;
        let message_id = MessageId::mint(&self.environment)?;

        let mut scope = TxScope {
            tx: self.begin_root_write().await.map_err(driver)?,
            entropy: &self.environment,
            opened_direct_pairs: Vec::new(),
        };
        match scope.write(command, message_id, created_at_ms).await {
            Ok(outcome) => {
                // The outcome only escapes a COMMITTED unit: this is the boundary that matters,
                // not where the value happened to be built.
                scope.tx.commit().await.map_err(driver)?;
                Ok(outcome)
            }
            Err(refusal) => {
                // The refusal is the answer: a rollback statement that itself
                // fails must not replace it.
                drop(scope.tx.rollback().await);
                Err(refusal)
            }
        }
    }
}

impl<E: SendClock + SendEntropy + Send + Sync> MentionedViewerRecord for MessagingStore<E> {
    /// Writes the records this send owes the people it named — deciding, HERE, who
    /// those names belong to and who may still read the conversation.
    ///
    /// One transaction of its own, taken after the send has committed: the entitlement
    /// read and every insert share it, so the rows that exist are the rows the world
    /// looked like when they were written. A person removed before this runs is not a
    /// candidate; a person removed after it commits keeps a record of something they
    /// could read at the time.
    async fn record_mentioned_viewers(
        &self,
        intent: &MentionIntent,
    ) -> Result<Vec<CommittedMentionRow>, EffectFailure> {
        self.record_mentions_transaction(intent)
            .await
            .map_err(EffectFailure::from_source)
    }
}

impl<E: SendClock + SendEntropy> MessagingStore<E> {
    #[tracing::instrument(skip(self, intent), fields(
        source_conversation = intent.source_conversation().as_str(),
        named = intent.handles().len(),
    ))]
    async fn record_mentions_transaction(
        &self,
        intent: &MentionIntent,
    ) -> Result<Vec<CommittedMentionRow>, MessagingStoreError> {
        let named = serde_json::to_string(
            &intent
                .handles()
                .iter()
                .map(ParticipantHandle::as_str)
                .collect::<Vec<_>>(),
        )?;
        let payload_of = |mentioned: &str| {
            serde_json::to_string(&serde_json::json!({
                "conversation_title": intent.source_conversation_title(),
                "trigger_name": intent.trigger_name(),
                "mentioned_name": mentioned,
                "mention_excerpt": intent.excerpt(),
            }))
        };

        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;
        let entitled = sqlx::query(SELECT_ENTITLED_VIEWERS)
            .bind(named)
            .bind(intent.author().map(UserId::as_str))
            .bind(intent.source_conversation().as_str())
            .bind(intent.source_workspace().as_str())
            .fetch_all(&mut *tx)
            .await?;

        let mut rows = Vec::with_capacity(entitled.len());
        for row in entitled {
            let viewer: String = row.try_get("id")?;
            let mentioned: String = row.try_get("name")?;
            let id = NotificationId::mint(&self.environment)?;
            let created_at_ms = self.environment.now_unix_ms()?;

            sqlx::query(INSERT_MENTION_SELF)
                .bind(id.as_str())
                .bind(&viewer)
                .bind(intent.source_workspace().as_str())
                .bind(intent.source_conversation().as_str())
                .bind(intent.source_message().as_str())
                .bind(intent.source_workspace().as_str())
                .bind(intent.source_conversation().as_str())
                .bind(intent.source_message().as_str())
                .bind(payload_of(&mentioned)?)
                .bind(created_at_ms)
                .execute(&mut *tx)
                .await?;

            rows.push(CommittedMentionRow::committed(
                id,
                created_at_ms,
                UserId::parse(viewer)
                    .map_err(|_| MessagingStoreError::MalformedIdentity { field: "users.id" })?,
                ParticipantHandle::parse(mentioned).map_err(|_| {
                    MessagingStoreError::MalformedIdentity {
                        field: "users.name",
                    }
                })?,
                intent.clone(),
            ));
        }

        // Nothing is reported until every row of this batch is durable: a partial
        // report would tell some people about a record and leave others' behind.
        tx.commit().await?;
        Ok(rows)
    }
}

impl<E: SendClock + SendEntropy + Send + Sync> SendMessageUnitOfWork for MessagingStore<E> {
    async fn send_message(
        &self,
        command: SendMessage,
    ) -> Result<SendMessageOutcome, SendMessageError> {
        self.send_message_transaction(command).await
    }
}

/// The transaction's own scope. Every fact the write depends on is read through
/// it, so a concurrent change cannot land between a check and the insert.
struct TxScope<'connection, 'entropy, E> {
    tx: Transaction<'connection, Sqlite>,
    entropy: &'entropy E,
    opened_direct_pairs:
        Vec<silverhand_backend_core::conversation_messaging::messaging::DirectPairOpenedFact>,
}

#[derive(Default)]
struct RecordedDirectPairOpens(
    Mutex<Vec<silverhand_backend_core::conversation_messaging::messaging::DirectPairOpenedFact>>,
);

impl RecordedDirectPairOpens {
    fn into_facts(
        self,
    ) -> Vec<silverhand_backend_core::conversation_messaging::messaging::DirectPairOpenedFact> {
        self.0.into_inner().expect("direct-pair recorder poisoned")
    }
}

impl DirectPairOpenedPort for RecordedDirectPairOpens {
    type Error = Infallible;

    async fn direct_pair_opened(
        &self,
        fact: &silverhand_backend_core::conversation_messaging::messaging::DirectPairOpenedFact,
    ) -> Result<(), Self::Error> {
        self.0
            .lock()
            .expect("direct-pair recorder poisoned")
            .push(fact.clone());
        Ok(())
    }
}

struct TransactionDirectPairMint<'scope, 'connection, 'entropy, E> {
    scope: &'scope mut TxScope<'connection, 'entropy, E>,
}

impl<E: SendEntropy + Send + Sync> DirectPairMint for TransactionDirectPairMint<'_, '_, '_, E> {
    type Error = SendMessageError;

    async fn mint_direct_pair(
        &mut self,
        participants: &DirectPairParticipants,
        now_unix_ms: i64,
    ) -> Result<DirectPairMinted, Self::Error> {
        let minted = mint_conversation_id(self.scope.entropy)?;
        mint_direct_pair_on(&mut self.scope.tx, minted, participants, now_unix_ms)
            .await
            .map_err(infrastructure)
    }
}

impl<E: SendEntropy + Send + Sync> TxScope<'_, '_, E> {
    async fn write(
        &mut self,
        command: SendMessage,
        message_id: MessageId,
        created_at_ms: i64,
    ) -> Result<SendMessageOutcome, SendMessageError> {
        // The sender's own binding must still exist. Read for existence only: what
        // the send is authorized against is the target.
        if command.source_conversation() != command.target_conversation()
            && self
                .conversation_facts(command.source_conversation())
                .await?
                .is_none()
        {
            return Err(ConversationRefusal::NotFound.into());
        }

        let facts =
            messaging::assert_post_authority(self, command.target_conversation(), command.actor())
                .await?;

        if let Some(replayed) = self.replay_client_message(&command).await? {
            return Ok(replayed);
        }

        let thread_root = self
            .resolve_thread_root(&facts.id, command.destination())
            .await?;
        let snapshots = self
            .resolve_attachments(&facts.id, command.attachment_refs())
            .await?;
        let freshness_bypassed = self.assert_fresh(&command, &facts.id).await?;

        let seq = self.reserve_next_seq(&facts.id).await?;
        let author = self.author_snapshot(command.actor()).await?;
        let message = self
            .insert_message(
                &command,
                &facts.id,
                message_id,
                created_at_ms,
                thread_root,
                seq,
                author,
                snapshots,
            )
            .await?;
        if let Some(id) = command.client_message_id() {
            sqlx::query(
                "UPDATE messages SET client_message_id=?, client_request_json=? WHERE id=?",
            )
            .bind(id.as_str())
            .bind(client_request_json(&command))
            .bind(message.id.as_str())
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?;
        }
        self.bump_stream_head(&facts.id, message.thread_root_id.as_ref(), seq)
            .await?;

        let thread = match &message.thread_root_id {
            None => None,
            Some(root) => {
                // A cluster's durable consequences belong to this transaction: a
                // follower recorded without its catch-up would leave an agent
                // following a conversation whose beginning it cannot see.
                self.apply_cluster_follow_effects(&facts.id, root, &command, seq, created_at_ms)
                    .await?;
                // The list entry is projected HERE, inside the same transaction that
                // wrote this reply, so the value carried on the fact (and borrowed by
                // publication and the ACK) is the committed one — not something a
                // post-commit reread might find changed or be unable to answer. This
                // reply is itself a non-system participant of the cluster, so the
                // projection always finds it; an absent entry is a broken invariant.
                let list_entry = project_list_entry(&mut self.tx, &facts.id, root)
                    .await?
                    .ok_or_else(|| infrastructure(MessagingStoreError::MissingClusterProjection))?;
                Some(ThreadReplyFact::committed(list_entry))
            }
        };

        let mention_copies = self
            .apply_mention_copies(&command, &facts.id, &message, created_at_ms)
            .await?;
        // Planned last, so the enumeration sees every row this unit wrote — a
        // follower this very reply created is part of who it is for.
        let post_commit = self
            .plan_post_commit(command.source_conversation(), &facts.id, &command, &message)
            .await?;

        // The current message's durable delivery witness rides THIS transaction. The
        // recipients are read back from the plan the aftermath will consume — never
        // recomputed — so the queued set and the dispatched set are the same fact by
        // construction. A recipient owed this message therefore holds a `turn` row
        // before the send is committed: a crash between commit and dispatch cannot
        // lose it, and the daemon's claim finds it rather than the empty `NoPending`
        // it would otherwise swallow. Historical catch-up (`observe`) is queued by the
        // cluster-follow effects above; this writes only the message this send made,
        // and the main-line and reply arms stay separate so each path's witness has
        // its own producer.
        match post_commit.delivery() {
            DeliveryPlan::MainLine(plan) => {
                for recipient in plan.recipients() {
                    self.ensure_delivery(
                        &facts.id,
                        recipient,
                        message.id.as_str(),
                        DELIVERY_KIND_TURN,
                        created_at_ms,
                    )
                    .await?;
                }
            }
            DeliveryPlan::ReplyCluster(audience) => {
                for recipient in audience.recipients() {
                    self.ensure_delivery(
                        &facts.id,
                        recipient,
                        message.id.as_str(),
                        DELIVERY_KIND_TURN,
                        created_at_ms,
                    )
                    .await?;
                }
            }
        }

        // Last, so it sees the copies this send actually made and the roster as of this unit.
        let unresolved = self
            .unresolved_references(&command, &facts.id, &mention_copies)
            .await?;

        // Assembled INSIDE the transaction, like the fact itself; whether it ever reaches a
        // caller is the wrapper's decision, and on a rollback it does not. Whether a local value
        // has been constructed is not an observable property of a transaction.
        Ok(SendMessageOutcome::committed_send(
            MessageCreatedFact::committed(
                message,
                thread,
                post_commit,
                freshness_bypassed,
                mention_copies,
            ),
            unresolved,
        ))
    }

    // Called after the same mutable post-authority gate as a new send, under
    // BEGIN IMMEDIATE. A replay neither reserves a sequence nor constructs effects.
    async fn replay_client_message(
        &mut self,
        command: &SendMessage,
    ) -> Result<Option<SendMessageOutcome>, SendMessageError> {
        let Some(token) = command.client_message_id() else {
            return Ok(None);
        };
        let SendActor::User(user) = command.actor() else {
            return Err(SendMessageError::ClientMessageConflict);
        };
        let row =
            sqlx::query("SELECT * FROM messages WHERE from_user_id=? AND client_message_id=?")
                .bind(user.as_str())
                .bind(token.as_str())
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(driver)?;
        let Some(row) = row else { return Ok(None) };
        let request: Option<String> = row.try_get("client_request_json").map_err(driver)?;
        if request.as_deref() != Some(client_request_json(command).as_str()) {
            return Err(SendMessageError::ClientMessageConflict);
        }
        let metadata: Option<String> = row.try_get("metadata").map_err(driver)?;
        let attachments =
            collaboration::conversation_message_window::decode_attachments(metadata.as_deref())
                .map_err(|e| SendMessageError::Persistence(PersistenceFailure::from_source(e)))?;
        let root: Option<String> = row.try_get("thread_root_id").map_err(driver)?;
        let message = CreatedMessage {
            client_message_id: Some(token.as_str().to_owned()),
            id: MessageId::parse(row.try_get::<String, _>("id").map_err(driver)?)
                .map_err(|_| SendMessageError::ClientMessageConflict)?,
            conversation_id: command.target_conversation().clone(),
            from_kind: FromKind::User,
            from_agent_id: None,
            from_user_id: Some(user.clone()),
            author: self.author_snapshot(command.actor()).await?,
            content: MessageContent::parse(row.try_get::<String, _>("content").map_err(driver)?)
                .map_err(|_| SendMessageError::ClientMessageConflict)?,
            attachments,
            seq: MessageSeq::try_from(row.try_get::<i64, _>("seq").map_err(driver)?)
                .map_err(|_| SendMessageError::ClientMessageConflict)?,
            created_at_ms: row.try_get("created_at").map_err(driver)?,
            thread_root_id: root
                .map(ThreadRootId::parse)
                .transpose()
                .map_err(|_| SendMessageError::ClientMessageConflict)?,
        };
        Ok(Some(
            SendMessageOutcome::committed_send(
                MessageCreatedFact::committed(
                    message,
                    None,
                    PostCommitPlan::committed(
                        None,
                        DeliveryPlan::MainLine(MainDeliveryPlan::committed(Vec::new(), Vec::new())),
                        Vec::new(),
                    ),
                    false,
                    Vec::new(),
                ),
                Vec::new(),
            )
            .as_replay(),
        ))
    }

    /// Which of this send's references reached nobody, decided INSIDE the unit that committed it.
    ///
    /// Asked here rather than after the commit because the answer is only true of the roster this
    /// send saw: a membership change a moment later would make the diagnostic disagree with a
    /// message that is already a fact.
    ///
    /// A reference is REACHED if the principal still participates in the TARGET conversation, or if
    /// this very send copied the mention to that agent. Both halves matter: an agent outside the
    /// target can still be reached through the human author ↔ agent DirectPair when eligible
    /// for CC, and a participant needs no copy.
    ///
    /// The copy outcomes are read for `Copied` ONLY — never for a skip reason. `AlreadyMember` asks
    /// about the SOURCE conversation, so on a cross-post it answers about the wrong roster.
    ///
    /// Unknown and resolvable-but-not-a-participant are ONE answer. The sender learns that the name
    /// reached nobody, never whether it exists — so this cannot become a way to probe the roster.
    async fn unresolved_references(
        &mut self,
        command: &SendMessage,
        target: &ConversationId,
        copies: &[mention_cc::MentionCopyOutcome],
    ) -> Result<Vec<MessageReference>, SendMessageError> {
        // AGENT handles, and the kind is half of the key. One body may carry both
        // `<silverhand://agent/Alice>` and `<silverhand://human/Alice>`; a copy made for the agent
        // says nothing about the human token, and matching on the handle alone would silently
        // report the human one as reached.
        let copied_agents: HashSet<&str> = copies
            .iter()
            .filter_map(|outcome| match outcome {
                mention_cc::MentionCopyOutcome::Copied { handle, .. } => Some(handle.as_str()),
                mention_cc::MentionCopyOutcome::Skipped { .. } => None,
            })
            .collect();

        let mut unresolved = Vec::new();
        for reference in command.addressed().iter() {
            if reference.kind() == PrincipalKind::Agent
                && copied_agents.contains(reference.handle().as_str())
            {
                continue;
            }
            // The reference's own kind decides which namespace answers for the handle: a human
            // reference must never resolve to an agent of the same name.
            let statement = match reference.kind() {
                PrincipalKind::Agent => SELECT_AGENT_ID_BY_NAME,
                PrincipalKind::Human => SELECT_USER_ID_BY_NAME,
            };
            let resolved: Option<String> = sqlx::query_scalar(statement)
                .bind(reference.handle().as_str())
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(driver)?;

            let reached = match resolved {
                None => false,
                Some(id) => match actor_of(reference.kind(), id)? {
                    SendActor::Agent(agent) => self.is_agent_participant(target, &agent).await?,
                    SendActor::User(user) => self.is_user_participant(target, &user).await?,
                },
            };
            if !reached {
                unresolved.push(reference.clone());
            }
        }
        Ok(unresolved)
    }

    /// Resolves the cluster this send belongs to, checking the caller's name for it
    /// here rather than trusting that some earlier layer did.
    async fn resolve_thread_root(
        &mut self,
        conversation: &ConversationId,
        destination: &SendDestination,
    ) -> Result<Option<ThreadRootId>, SendMessageError> {
        match destination {
            SendDestination::MainLine => Ok(None),
            // A named root must be a message of this conversation that starts a
            // cluster. Unknown, foreign, and "is itself a reply" are one answer.
            SendDestination::Thread(root) => {
                let row = self
                    .message_in_conversation(conversation, root.as_str())
                    .await?
                    .ok_or(ThreadRootRefusal)?;
                if row.thread_root_id.is_some() {
                    return Err(ThreadRootRefusal.into());
                }
                Ok(Some(root.clone()))
            }
            // A reply names a message; the cluster is that message's own root, or
            // the message itself when it starts one.
            SendDestination::Reply(target) => {
                let row = self
                    .message_in_conversation(conversation, target.as_str())
                    .await?
                    .ok_or(ReplyTargetRefusal)?;
                let root = row.thread_root_id.unwrap_or(row.id);
                Ok(Some(ThreadRootId::parse(root).map_err(|_| {
                    infrastructure(MessagingStoreError::MalformedIdentity {
                        field: "messages.id",
                    })
                })?))
            }
        }
    }

    async fn message_in_conversation(
        &mut self,
        conversation: &ConversationId,
        message_id: &str,
    ) -> Result<Option<MessageRow>, SendMessageError> {
        let row = sqlx::query(SELECT_MESSAGE_IN_CONVERSATION)
            .bind(conversation.as_str())
            .bind(message_id)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        let Some(row) = row else { return Ok(None) };

        Ok(Some(MessageRow {
            id: row.try_get("id").map_err(driver)?,
            thread_root_id: row.try_get("thread_root_id").map_err(driver)?,
            from_kind: row.try_get("from_kind").map_err(driver)?,
            from_agent_id: row.try_get("from_agent_id").map_err(driver)?,
            from_user_id: row.try_get("from_user_id").map_err(driver)?,
        }))
    }

    async fn resolve_attachments(
        &mut self,
        conversation: &ConversationId,
        refs: &[messaging::attachments::AttachmentRef],
    ) -> Result<Vec<AttachmentSnapshot>, SendMessageError> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }

        // Looked up one id at a time: an `IN` list would have to be assembled as
        // SQL text, and the decision below stops at the first bad reference
        // anyway, so a batched read would buy nothing it can use.
        let mut sources = Vec::with_capacity(refs.len());
        for reference in refs {
            let row = sqlx::query(SELECT_ATTACHMENT_SOURCE)
                .bind(reference.id.as_str())
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(driver)?;
            let Some(row) = row else { continue };

            sources.push(AttachmentSource {
                id: reference.id.clone(),
                channel_id: ConversationId::parse(
                    row.try_get::<String, _>("channel_id").map_err(driver)?,
                )
                .map_err(|_| {
                    infrastructure(MessagingStoreError::MalformedIdentity {
                        field: "attachments.channel_id",
                    })
                })?,
                filename: row.try_get("filename").map_err(driver)?,
                size: row.try_get("size").map_err(driver)?,
                content_type: row.try_get("content_type").map_err(driver)?,
            });
        }

        Ok(messaging::attachments::resolve_attachment_snapshots(
            conversation,
            refs,
            &sources,
        )?)
    }

    /// Returns whether the guard was skipped at the sender's request.
    async fn assert_fresh(
        &mut self,
        command: &SendMessage,
        target: &ConversationId,
    ) -> Result<bool, SendMessageError> {
        let agent =
            match messaging::freshness::freshness_gate(command.actor(), command.freshness_bypass())
            {
                FreshnessGate::NotApplicable => return Ok(false),
                FreshnessGate::Bypassed => return Ok(true),
                FreshnessGate::Check(agent) => agent,
            };

        let declared: Option<String> = sqlx::query_scalar(SELECT_AGENT_SESSION_SCOPE)
            .bind(agent.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        let declared = declared.ok_or_else(|| FreshnessGuardInvariant::AgentAbsent {
            agent: agent.clone(),
        })?;
        let scope = AgentSessionScope::parse(&declared).ok_or_else(|| {
            FreshnessGuardInvariant::UnknownSessionScope {
                agent: agent.clone(),
            }
        })?;

        // The conversation arm keys on the SOURCE conversation: freshness is about
        // what this runtime has not yet consumed, not about where it is writing.
        let undelivered: Option<i64> = match scope {
            AgentSessionScope::Conversation => {
                sqlx::query_scalar(SELECT_UNDELIVERED_IN_CONVERSATION)
                    .bind(command.source_conversation().as_str())
                    .bind(agent.as_str())
                    .fetch_optional(&mut *self.tx)
                    .await
                    .map_err(driver)?
            }
            AgentSessionScope::Agent => sqlx::query_scalar(SELECT_UNDELIVERED_FOR_AGENT)
                .bind(agent.as_str())
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(driver)?,
        };

        if undelivered.is_some() {
            return Err(messaging::StaleRefusal {
                stale_event_id: StaleEventId::mint(self.entropy)?,
                target_conversation_id: target.clone(),
            }
            .into());
        }
        Ok(false)
    }

    /// Advances the conversation's counter and reads back what it reserved. The
    /// counter is conversation-global: a reply and a main-line message draw from
    /// it alike, which is what makes `(conversation, ordinal)` a total order.
    async fn reserve_next_seq(
        &mut self,
        conversation: &ConversationId,
    ) -> Result<MessageSeq, SendMessageError> {
        reserve_next_seq_on(&mut self.tx, conversation)
            .await
            .map_err(|fault| match fault {
                ReserveSeqFault::Driver(error) => driver(error).into(),
                ReserveSeqFault::Ordinal(error) => {
                    infrastructure(MessagingStoreError::Ordinal(error))
                }
            })
    }

    /// The author the message reports, resolved through the adapter's one author
    /// authority ([`resolve_authors`]) as a ONE-ELEMENT batch — the same authority a
    /// list read uses for a whole page, so a point read invents no second read or
    /// discrimination path of its own. At send the actor is current, so this resolves
    /// to an active author; its snapshot columns are then copied onto the row so the
    /// message keeps naming its author after a rename or a delete.
    async fn author_snapshot(
        &mut self,
        actor: &SendActor,
    ) -> Result<Option<MessageAuthor>, SendMessageError> {
        let evidence = match actor {
            SendActor::Agent(agent) => AuthorEvidence {
                from_kind: FromKind::Agent,
                from_agent_id: Some(agent.as_str().to_owned()),
                from_user_id: None,
                // Not yet deleted: the snapshot columns are what this send is about
                // to write, so resolution reads the live row, not them.
                author_id: None,
                author_name: None,
                author_display_name: None,
            },
            SendActor::User(user) => AuthorEvidence {
                from_kind: FromKind::User,
                from_agent_id: None,
                from_user_id: Some(user.as_str().to_owned()),
                author_id: None,
                author_name: None,
                author_display_name: None,
            },
        };
        let mut resolved = resolve_authors(&mut self.tx, std::slice::from_ref(&evidence))
            .await
            .map_err(SendMessageError::from)?;
        Ok(resolved.pop().flatten())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_message(
        &mut self,
        command: &SendMessage,
        conversation: &ConversationId,
        id: MessageId,
        created_at_ms: i64,
        thread_root: Option<ThreadRootId>,
        seq: MessageSeq,
        author: Option<MessageAuthor>,
        attachments: Vec<AttachmentSnapshot>,
    ) -> Result<CreatedMessage, SendMessageError> {
        let metadata = metadata_json(&attachments)?;
        let (from_kind, agent_id, user_id) = match command.actor() {
            SendActor::Agent(agent) => (FROM_KIND_AGENT, Some(agent.as_str()), None),
            SendActor::User(user) => (FROM_KIND_USER, None, Some(user.as_str())),
        };
        let (author_id, author_name, author_display_name) =
            author_snapshot_columns(author.as_ref());

        sqlx::query(INSERT_MESSAGE)
            .bind(id.as_str())
            .bind(conversation.as_str())
            .bind(from_kind)
            .bind(agent_id)
            .bind(user_id)
            .bind(command.content().as_str())
            .bind(metadata)
            .bind(seq.get())
            .bind(created_at_ms)
            .bind(thread_root.as_ref().map(ThreadRootId::as_str))
            .bind(author_id)
            .bind(author_name)
            .bind(author_display_name)
            .bind(None::<&str>)
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?;

        Ok(CreatedMessage {
            client_message_id: command.client_message_id().map(|id| id.as_str().to_owned()),
            id,
            conversation_id: conversation.clone(),
            from_kind: command.actor().from_kind(),
            from_agent_id: match command.actor() {
                SendActor::Agent(agent) => Some(agent.clone()),
                SendActor::User(_) => None,
            },
            from_user_id: match command.actor() {
                SendActor::User(user) => Some(user.clone()),
                SendActor::Agent(_) => None,
            },
            author,
            content: command.content().clone(),
            attachments,
            seq,
            created_at_ms,
            thread_root_id: thread_root,
        })
    }

    /// The head is advanced unconditionally: it records which ordinal a stream last
    /// reached, and is not a token anything competes for.
    async fn bump_stream_head(
        &mut self,
        conversation: &ConversationId,
        thread_root: Option<&ThreadRootId>,
        seq: MessageSeq,
    ) -> Result<(), SendMessageError> {
        bump_stream_head_on(&mut self.tx, conversation, thread_root, seq)
            .await
            .map_err(|error| driver(error).into())
    }

    /// Copies this message to each agent it addressed that cannot read the
    /// conversation it was written in, onto the direct line between them and the
    /// author — minting that line if it does not exist yet.
    ///
    /// The copy, its delivery row and the author's notification commit with the
    /// message that caused them: a copy that could be lost while the mention stands
    /// would leave the author believing they had reached someone they had not.
    ///
    /// Only a person's mention carries this promise. An agent's does not, so its
    /// send addresses no copies at all.
    async fn apply_mention_copies(
        &mut self,
        command: &SendMessage,
        source: &ConversationId,
        message: &CreatedMessage,
        created_at_ms: i64,
    ) -> Result<Vec<MentionCopyOutcome>, SendMessageError> {
        let SendActor::User(author) = command.actor() else {
            return Ok(Vec::new());
        };
        let addressed: Vec<ParticipantHandle> = command
            .addressed()
            .iter()
            .filter(|reference| reference.kind() == PrincipalKind::Agent)
            .map(|reference| reference.handle().clone())
            .collect();
        if addressed.is_empty() {
            return Ok(Vec::new());
        }

        // A copy is named after where it came from, and a direct line has no
        // workspace to be named after.
        let (workspace_id, source_title) = self.conversation_place(source).await?;
        let Some(workspace_id) = workspace_id else {
            return Ok(addressed
                .into_iter()
                .map(|handle| MentionCopyOutcome::Skipped {
                    handle,
                    reason: MentionCopySkip::SourceNotWorkspacePlaced,
                    agent: None,
                })
                .collect());
        };
        let workspace_name = self.workspace_name(&workspace_id).await?;

        // No "already copied" arm: the addressed set is deduplicated where it is
        // derived, and one handle names one agent, so a second copy to the same
        // agent has no way to be asked for.
        let mut outcomes = Vec::with_capacity(addressed.len());
        for handle in addressed {
            let Some(agent) = self.agent_by_name(&handle).await? else {
                outcomes.push(MentionCopyOutcome::Skipped {
                    handle,
                    reason: MentionCopySkip::UnknownHandle,
                    agent: None,
                });
                continue;
            };
            // Already a participant: it received the original, and a copy would be
            // the same message twice.
            if self.is_agent_participant(source, &agent).await? {
                outcomes.push(MentionCopyOutcome::Skipped {
                    handle,
                    reason: MentionCopySkip::AlreadyMember,
                    agent: Some(agent),
                });
                continue;
            }
            // The handle was resolved to an id HERE, inside the unit, and the
            // verdict was computed before it: a name that now points at a different
            // agent than it did then is not granted, and the copy does not happen.
            if !command.direct_lines().contains(&agent) {
                outcomes.push(MentionCopyOutcome::Skipped {
                    handle,
                    reason: MentionCopySkip::NoDirectLine,
                    agent: Some(agent),
                });
                continue;
            }

            let place = MentionCopyPlace {
                workspace_id: &workspace_id,
                workspace_name: &workspace_name,
                source: source.clone(),
                source_title: &source_title,
            };
            let line = self.open_direct_pair(author, &agent, created_at_ms).await?;
            let copy = self
                .write_mention_copy(&line, &handle, &place, message, author, created_at_ms)
                .await?;

            self.ensure_delivery(
                &line,
                &agent,
                copy.id.as_str(),
                DELIVERY_KIND_TURN,
                created_at_ms,
            )
            .await?;
            self.notify_author_of_copy(author, &agent, &place, message, created_at_ms)
                .await?;

            outcomes.push(MentionCopyOutcome::Copied {
                handle,
                agent,
                copy: Box::new(copy),
            });
        }
        Ok(outcomes)
    }

    /// Decides what this send owes the world, from the same snapshot it was written
    /// in.
    ///
    /// Two layers: who was named is a fact of the body and does not depend on the
    /// path, while who receives the message does — a reply reaches its cluster's
    /// followers, a main-line message reaches the conversation's participants.
    async fn plan_post_commit(
        &mut self,
        source: &ConversationId,
        target: &ConversationId,
        command: &SendMessage,
        message: &CreatedMessage,
    ) -> Result<PostCommitPlan, SendMessageError> {
        let mentions = self.mention_intent(target, command, message).await?;
        let opened_direct_pairs = std::mem::take(&mut self.opened_direct_pairs);

        if let Some(root) = &message.thread_root_id {
            let audience = self.cluster_audience(root, command.actor()).await?;
            return Ok(PostCommitPlan::committed(
                mentions,
                DeliveryPlan::ReplyCluster(ReplyAudience::committed(audience)),
                opened_direct_pairs,
            ));
        }

        // Three cases, and they differ in both answers. An agent writing into its own
        // conversation is not delivered its own message, and starts nobody: the
        // runtime that would be started is the one already running to write this. An
        // agent writing into ANOTHER conversation is not the same actor there — that
        // conversation's own session is a recipient, and idle recipients are started.
        // A person's send excludes nobody and starts whoever is idle.
        let writes_elsewhere = source != target;
        let (excluded, starts) = match command.actor() {
            SendActor::Agent(agent) if !writes_elsewhere => {
                (Some(agent.as_str().to_owned()), false)
            }
            SendActor::Agent(_) => (None, true),
            SendActor::User(_) => (None, true),
        };

        let participants: Vec<String> = sqlx::query_scalar(SELECT_PARTICIPANT_AGENTS)
            .bind(target.as_str())
            .fetch_all(&mut *self.tx)
            .await
            .map_err(driver)?;
        let recipients: Vec<AgentId> = participants
            .into_iter()
            .filter(|candidate| excluded.as_deref() != Some(candidate.as_str()))
            .map(|candidate| {
                AgentId::parse(candidate).map_err(|_| {
                    infrastructure(MessagingStoreError::MalformedIdentity { field: "agents.id" })
                })
            })
            .collect::<Result<_, _>>()?;
        let start_eligible = if starts {
            recipients.clone()
        } else {
            Vec::new()
        };

        Ok(PostCommitPlan::committed(
            mentions,
            DeliveryPlan::MainLine(MainDeliveryPlan::committed(recipients, start_eligible)),
            opened_direct_pairs,
        ))
    }

    /// What this send says about naming people, if it named anyone inside a workspace.
    ///
    /// Only the facts of the send: the handles the body named, where it happened, who
    /// wrote it, and what it said. Who is entitled to be told is NOT decided here —
    /// that is a fact of the moment the record is written, and deciding it now would
    /// both freeze an entitlement that can be revoked in between and drag its reads
    /// into this transaction, where a failed lookup for a notification would roll back
    /// a message that was otherwise complete.
    ///
    /// Nothing is recorded outside a workspace: a mention record is navigated to by
    /// workspace and conversation, and a direct line has no workspace to reach it from.
    async fn mention_intent(
        &mut self,
        conversation: &ConversationId,
        command: &SendMessage,
        message: &CreatedMessage,
    ) -> Result<Option<MentionIntent>, SendMessageError> {
        let handles: Vec<ParticipantHandle> = command
            .addressed()
            .iter()
            .filter(|reference| reference.kind() == PrincipalKind::Human)
            .map(|reference| reference.handle().clone())
            .collect();
        if handles.is_empty() {
            return Ok(None);
        }

        let (workspace_id, title) = self.conversation_place(conversation).await?;
        let Some(workspace_id) = workspace_id else {
            return Ok(None);
        };
        Ok(Some(MentionIntent::committed(
            handles,
            match command.actor() {
                SendActor::User(author) => Some(author.clone()),
                SendActor::Agent(_) => None,
            },
            trigger_name_of(message),
            workspace_id,
            conversation.clone(),
            title,
            message.id.clone(),
            mention_cc::mention_excerpt(message.content.as_str()),
        )))
    }

    async fn conversation_place(
        &mut self,
        conversation: &ConversationId,
    ) -> Result<(Option<WorkspaceId>, String), SendMessageError> {
        let row = sqlx::query(SELECT_CONVERSATION_PLACE)
            .bind(conversation.as_str())
            .fetch_one(&mut *self.tx)
            .await
            .map_err(driver)?;
        let workspace_id: Option<String> = row.try_get("workspace_id").map_err(driver)?;
        let workspace_id = workspace_id
            .map(WorkspaceId::parse)
            .transpose()
            .map_err(|_| {
                infrastructure(MessagingStoreError::MalformedIdentity {
                    field: "conversations.workspace_id",
                })
            })?;
        Ok((workspace_id, row.try_get("title").map_err(driver)?))
    }

    async fn workspace_name(
        &mut self,
        workspace: &WorkspaceId,
    ) -> Result<String, SendMessageError> {
        let name: Option<String> = sqlx::query_scalar(SELECT_WORKSPACE_NAME)
            .bind(workspace.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        name.ok_or_else(|| {
            infrastructure(MessagingStoreError::MalformedIdentity {
                field: "conversations.workspace_id",
            })
        })
    }

    async fn agent_by_name(
        &mut self,
        handle: &ParticipantHandle,
    ) -> Result<Option<AgentId>, SendMessageError> {
        let found: Option<String> = sqlx::query_scalar(SELECT_AGENT_ID_BY_NAME)
            .bind(handle.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        match found {
            None => Ok(None),
            Some(id) => Ok(Some(AgentId::parse(id).map_err(|_| {
                infrastructure(MessagingStoreError::MalformedIdentity { field: "agents.id" })
            })?)),
        }
    }

    /// Gets or creates the line between two principals. The endpoints are stored in
    /// one canonical order so the pair has a single identity whichever side is
    /// asking; the order is the database's own, which in Rust is just how strings
    /// compare.
    async fn open_direct_pair(
        &mut self,
        author: &UserId,
        agent: &AgentId,
        created_at_ms: i64,
    ) -> Result<ConversationId, SendMessageError> {
        let participants = DirectPairParticipants::between(
            PrincipalIdentity::Human(author.clone()),
            PrincipalIdentity::Agent(agent.clone()),
        )
        .expect("a human and an agent are distinct principals");
        let opened = RecordedDirectPairOpens::default();
        let result = {
            let mut mint = TransactionDirectPairMint { scope: self };
            open_direct_pair(&mut mint, &opened, participants, created_at_ms).await
        };
        match result {
            Ok(conversation) => {
                self.opened_direct_pairs.extend(opened.into_facts());
                Ok(conversation)
            }
            Err(OpenDirectPairError::Mint(error)) => Err(error),
            Err(OpenDirectPairError::Announcement(never)) => match never {},
            Err(OpenDirectPairError::Identity(error)) => {
                Err(PersistenceFailure::from_source(error).into())
            }
        }
    }

    /// Writes the copy into the line: the same three facts a send writes anywhere —
    /// the conversation's ordinal, the message, the stream head.
    async fn write_mention_copy(
        &mut self,
        line: &ConversationId,
        recipient: &ParticipantHandle,
        place: &MentionCopyPlace<'_>,
        message: &CreatedMessage,
        author: &UserId,
        created_at_ms: i64,
    ) -> Result<mention_cc::CommittedMentionCopy, SendMessageError> {
        let seq = self.reserve_next_seq(line).await?;
        let copy = mention_cc::CommittedMentionCopy {
            id: MessageId::mint(self.entropy)?,
            conversation_id: line.clone(),
            from_kind: messaging::FromKind::System,
            content: mention_cc::render_copy_body(&mention_cc::MentionCopySource {
                recipient_handle: recipient,
                source_workspace_name: place.workspace_name,
                source_conversation_title: place.source_title,
                source_content: message.content.as_str(),
            }),
            metadata: mention_cc::MentionCopyMetadata {
                subtype: mention_cc::MENTION_COPY_SUBTYPE,
                source_workspace_id: place.workspace_id.clone(),
                source_workspace_name: place.workspace_name.to_owned(),
                source_conversation_id: place.source.clone(),
                source_conversation_title: place.source_title.to_owned(),
                source_message_id: message.id.clone(),
                source_from_kind: messaging::FromKind::User,
                source_from_user_id: author.clone(),
                source_message_content: message.content.as_str().to_owned(),
            },
            seq,
            created_at_ms,
            cc_source_message_id: message.id.clone(),
        };

        // The row is written FROM the fact, so what was committed and what is
        // reported are the same value rather than two renderings of one intent.
        sqlx::query(INSERT_MENTION_COPY)
            .bind(copy.id.as_str())
            .bind(copy.conversation_id.as_str())
            .bind(from_kind_column(copy.from_kind))
            .bind(&copy.content)
            .bind(mention_copy_metadata(&copy.metadata)?)
            .bind(copy.seq.get())
            .bind(copy.created_at_ms)
            .bind(copy.cc_source_message_id.as_str())
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?;
        self.bump_stream_head(&copy.conversation_id, None, copy.seq)
            .await?;
        Ok(copy)
    }

    async fn notify_author_of_copy(
        &mut self,
        author: &UserId,
        agent: &AgentId,
        place: &MentionCopyPlace<'_>,
        message: &CreatedMessage,
        created_at_ms: i64,
    ) -> Result<(), SendMessageError> {
        let payload = mention_cc::MentionNotificationPayload::of(
            place.workspace_name,
            place.source_title,
            message.content.as_str(),
        );
        let id = MessageId::mint(self.entropy)?;

        sqlx::query(INSERT_MENTION_NOTIFICATION)
            .bind(id.as_str())
            .bind(author.as_str())
            .bind(agent.as_str())
            .bind(place.workspace_id.as_str())
            .bind(place.source.as_str())
            .bind(message.id.as_str())
            .bind(place.workspace_id.as_str())
            .bind(place.source.as_str())
            .bind(message.id.as_str())
            .bind(mention_notification_payload(&payload)?)
            .bind(created_at_ms)
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(())
    }

    /// Records who follows this cluster now, and catches up whoever is new to it.
    ///
    /// Three sources, in this order: the sender, the root's author, and every
    /// principal the reply addressed. A reference widens the cluster's audience but
    /// grants nothing — following is not a way into a conversation — so each
    /// candidate still has to pass participation and liveness.
    async fn apply_cluster_follow_effects(
        &mut self,
        conversation: &ConversationId,
        root: &ThreadRootId,
        command: &SendMessage,
        current_seq: MessageSeq,
        followed_at_ms: i64,
    ) -> Result<(), SendMessageError> {
        // The cluster's anchor is read once: every catch-up needs it, and one
        // transaction is one snapshot.
        let root_row = self
            .message_in_conversation(conversation, root.as_str())
            .await?;

        self.follow_actor(
            conversation,
            root,
            root_row.as_ref(),
            command.actor(),
            ThreadFollowerSource::Reply,
            current_seq,
            followed_at_ms,
        )
        .await?;

        if let Some(row) = &root_row
            && let Some(author) = actor_of_row(row)?
        {
            self.follow_actor(
                conversation,
                root,
                root_row.as_ref(),
                &author,
                ThreadFollowerSource::TargetAuthor,
                current_seq,
                followed_at_ms,
            )
            .await?;
        }

        for reference in command.addressed().iter() {
            // The reference's own kind decides which namespace answers for the
            // handle: a human reference must never resolve to an agent of the same
            // name.
            let statement = match reference.kind() {
                PrincipalKind::Agent => SELECT_AGENT_ID_BY_NAME,
                PrincipalKind::Human => SELECT_USER_ID_BY_NAME,
            };
            let resolved: Option<String> = sqlx::query_scalar(statement)
                .bind(reference.handle().as_str())
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(driver)?;
            let Some(resolved) = resolved else { continue };

            let addressed = actor_of(reference.kind(), resolved)?;
            self.follow_actor(
                conversation,
                root,
                root_row.as_ref(),
                &addressed,
                ThreadFollowerSource::Mention,
                current_seq,
                followed_at_ms,
            )
            .await?;
        }
        Ok(())
    }

    /// Participation and liveness are both required, and they are different
    /// questions: participation survives a departed principal on purpose so a
    /// survivor keeps reading, while a delivery row for a principal that no longer
    /// exists cannot be written at all.
    #[allow(clippy::too_many_arguments)]
    async fn follow_actor(
        &mut self,
        conversation: &ConversationId,
        root: &ThreadRootId,
        root_row: Option<&MessageRow>,
        actor: &SendActor,
        source: ThreadFollowerSource,
        current_seq: MessageSeq,
        followed_at_ms: i64,
    ) -> Result<(), SendMessageError> {
        let (kind, id) = match actor {
            SendActor::Agent(agent) => (PRINCIPAL_KIND_AGENT, agent.as_str()),
            SendActor::User(user) => (PRINCIPAL_KIND_HUMAN, user.as_str()),
        };

        let participates = match actor {
            SendActor::Agent(agent) => self.is_agent_participant(conversation, agent).await?,
            SendActor::User(user) => self.is_user_participant(conversation, user).await?,
        };
        if !participates {
            return Ok(());
        }
        let alive = match actor {
            SendActor::Agent(agent) => self.agent_exists(agent).await?,
            SendActor::User(user) => {
                self.endpoint_principal_exists(&ConversationEndpoint {
                    principal: messaging::PrincipalIdentity::Human(user.clone()),
                    deleted_at_ms: None,
                })
                .await?
            }
        };
        if !alive {
            return Ok(());
        }

        let followed = sqlx::query(FOLLOW_CLUSTER)
            .bind(root.as_str())
            .bind(conversation.as_str())
            .bind(kind)
            .bind(id)
            .bind(followed_at_ms)
            .bind(match source {
                ThreadFollowerSource::Reply => FOLLOWER_SOURCE_REPLY,
                ThreadFollowerSource::TargetAuthor => FOLLOWER_SOURCE_TARGET_AUTHOR,
                ThreadFollowerSource::Mention => FOLLOWER_SOURCE_MENTION,
            })
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?
            .rows_affected();

        // Only a NEW follower is caught up, and only an agent has anywhere to
        // catch up into: a person reads a conversation, an agent is delivered one.
        if followed == 1
            && let SendActor::Agent(agent) = actor
        {
            self.backfill_cluster_history(
                conversation,
                root_row,
                root,
                agent,
                current_seq,
                followed_at_ms,
            )
            .await?;
        }
        Ok(())
    }

    /// Queues the cluster as it stood before this reply: the root, which anchors it,
    /// and its most recent earlier replies. The agent's own messages are skipped
    /// because it has already seen what it said. The current reply is not queued
    /// here; delivering it is a separate act.
    #[allow(clippy::too_many_arguments)]
    async fn backfill_cluster_history(
        &mut self,
        conversation: &ConversationId,
        root_row: Option<&MessageRow>,
        root: &ThreadRootId,
        agent: &AgentId,
        current_seq: MessageSeq,
        queued_at_ms: i64,
    ) -> Result<(), SendMessageError> {
        if let Some(root_row) = root_row
            && !root_row.authored_by(agent)
        {
            self.ensure_delivery(
                conversation,
                agent,
                &root_row.id,
                DELIVERY_KIND_OBSERVE,
                queued_at_ms,
            )
            .await?;
        }

        let history: Vec<String> = sqlx::query_scalar(SELECT_CLUSTER_HISTORY)
            .bind(conversation.as_str())
            .bind(root.as_str())
            .bind(current_seq.get())
            .bind(agent.as_str())
            .bind(THREAD_BACKFILL_RECENT_REPLIES)
            .fetch_all(&mut *self.tx)
            .await
            .map_err(driver)?;
        for id in history {
            self.ensure_delivery(
                conversation,
                agent,
                &id,
                DELIVERY_KIND_OBSERVE,
                queued_at_ms,
            )
            .await?;
        }
        Ok(())
    }

    async fn ensure_delivery(
        &mut self,
        conversation: &ConversationId,
        agent: &AgentId,
        message_id: &str,
        delivery_kind: &'static str,
        queued_at_ms: i64,
    ) -> Result<(), SendMessageError> {
        if !conversation_allows_delivery(&mut self.tx, conversation)
            .await
            .map_err(driver)?
        {
            return Ok(());
        }
        sqlx::query(ENSURE_INBOX_DELIVERY)
            .bind(conversation.as_str())
            .bind(agent.as_str())
            .bind(message_id)
            .bind(delivery_kind)
            .bind(queued_at_ms)
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(())
    }

    /// Who this reply is for: the cluster's follower agents that still participate
    /// and still exist, minus the sender — nobody is delivered their own message.
    /// Read here because the answer must include the followers this transaction
    /// just created.
    async fn cluster_audience(
        &mut self,
        root: &ThreadRootId,
        actor: &SendActor,
    ) -> Result<Vec<AgentId>, SendMessageError> {
        let rows: Vec<String> = sqlx::query_scalar(SELECT_CLUSTER_AUDIENCE)
            .bind(root.as_str())
            .fetch_all(&mut *self.tx)
            .await
            .map_err(driver)?;

        let sender = match actor {
            SendActor::Agent(agent) => Some(agent.as_str()),
            SendActor::User(_) => None,
        };
        rows.into_iter()
            .filter(|candidate| sender != Some(candidate.as_str()))
            .map(|candidate| {
                AgentId::parse(candidate).map_err(|_| {
                    infrastructure(MessagingStoreError::MalformedIdentity {
                        field: "thread_followers.principal_id",
                    })
                })
            })
            .collect()
    }
}

/// The principal a stored row names as its author, or nothing when the row was
/// written by the system or its author is no longer named on it.
fn actor_of_row(row: &MessageRow) -> Result<Option<SendActor>, SendMessageError> {
    match row.from_kind.as_str() {
        FROM_KIND_AGENT => match &row.from_agent_id {
            Some(agent) => Ok(Some(actor_of(PrincipalKind::Agent, agent.clone())?)),
            None => Ok(None),
        },
        FROM_KIND_USER => match &row.from_user_id {
            Some(user) => Ok(Some(actor_of(PrincipalKind::Human, user.clone())?)),
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

fn actor_of(kind: PrincipalKind, id: String) -> Result<SendActor, SendMessageError> {
    let malformed = || {
        infrastructure(MessagingStoreError::MalformedIdentity {
            field: "messages author",
        })
    };
    Ok(match kind {
        PrincipalKind::Agent => SendActor::Agent(AgentId::parse(id).map_err(|_| malformed())?),
        PrincipalKind::Human => SendActor::User(UserId::parse(id).map_err(|_| malformed())?),
    })
}

/// Where a copy came from, carried together because every write of a copy needs all
/// of it.
struct MentionCopyPlace<'facts> {
    workspace_id: &'facts WorkspaceId,
    workspace_name: &'facts str,
    source: ConversationId,
    source_title: &'facts str,
}

fn from_kind_column(kind: messaging::FromKind) -> &'static str {
    match kind {
        messaging::FromKind::User => FROM_KIND_USER,
        messaging::FromKind::Agent => FROM_KIND_AGENT,
        messaging::FromKind::System => FROM_KIND_SYSTEM,
    }
}

/// The inverse of [`from_kind_column`], reading a stored provenance back. The column
/// is this store's own closed set, so a spelling it never writes is a broken
/// database rather than a caller's mistake.
fn from_kind_of(stored: &str) -> Result<FromKind, PersistenceFailure> {
    match stored {
        FROM_KIND_USER => Ok(FromKind::User),
        FROM_KIND_AGENT => Ok(FromKind::Agent),
        FROM_KIND_SYSTEM => Ok(FromKind::System),
        _ => Err(malformed("messages.from_kind")),
    }
}

/// A stored column that should name something usable names nothing — a broken
/// database the store owns, reported with the column that broke it.
fn malformed(field: &'static str) -> PersistenceFailure {
    PersistenceFailure::from_source(MessagingStoreError::MalformedIdentity { field })
}

fn mention_copy_metadata(
    metadata: &mention_cc::MentionCopyMetadata,
) -> Result<String, SendMessageError> {
    serde_json::to_string(&serde_json::json!({
        "subtype": metadata.subtype,
        "source_workspace_id": metadata.source_workspace_id.as_str(),
        "source_workspace_name": metadata.source_workspace_name,
        "source_conversation_id": metadata.source_conversation_id.as_str(),
        "source_conversation_title": metadata.source_conversation_title,
        "source_message_id": metadata.source_message_id.as_str(),
        "source_from_kind": from_kind_column(metadata.source_from_kind),
        "source_from_user_id": metadata.source_from_user_id.as_str(),
        "source_message_content": metadata.source_message_content,
    }))
    .map_err(|error| infrastructure(MessagingStoreError::Serialization(error)))
}

fn mention_notification_payload(
    payload: &mention_cc::MentionNotificationPayload<'_>,
) -> Result<String, SendMessageError> {
    serde_json::to_string(&serde_json::json!({
        "source_workspace_name": payload.source_workspace_name,
        "source_conversation_title": payload.source_conversation_title,
        "mention_excerpt": payload.mention_excerpt,
    }))
    .map_err(|error| infrastructure(MessagingStoreError::Serialization(error)))
}

/// Who named someone, as a reader will see it. The sender's own snapshot, taken
/// inside the transaction: display name if it says anything, then name, then the id —
/// an id is not friendly, but it is never absent. An unresolved author names no one,
/// so it contributes nothing, exactly as an absent author does.
fn trigger_name_of(message: &CreatedMessage) -> String {
    let Some(author) = &message.author else {
        return String::new();
    };
    let (display, name, id): (&str, &str, Option<&str>) = match author {
        MessageAuthor::Active(ActiveAuthor::Agent(agent)) => (
            agent.display_name(),
            agent.name(),
            Some(agent.id().as_str()),
        ),
        MessageAuthor::Active(ActiveAuthor::Human(human)) => (
            human.display_name(),
            human.name(),
            Some(human.id().as_str()),
        ),
        MessageAuthor::Deleted(deleted) => (
            deleted.display_name(),
            deleted.name(),
            Some(deleted.identity().as_str()),
        ),
        MessageAuthor::Unresolved(_) => ("", "", None),
    };
    let display = display.trim();
    if !display.is_empty() {
        return display.to_owned();
    }
    let name = name.trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    id.unwrap_or("").to_owned()
}

struct MessageRow {
    id: String,
    thread_root_id: Option<String>,
    from_kind: String,
    from_agent_id: Option<String>,
    from_user_id: Option<String>,
}

impl MessageRow {
    fn authored_by(&self, agent: &AgentId) -> bool {
        self.from_kind == FROM_KIND_AGENT && self.from_agent_id.as_deref() == Some(agent.as_str())
    }
}

impl<E: Send + Sync> EndpointPrincipalReads for TxScope<'_, '_, E> {
    async fn endpoint_allows_direct_messages(
        &mut self,
        endpoint: &ConversationEndpoint,
    ) -> Result<bool, PersistenceFailure> {
        match &endpoint.principal {
            messaging::PrincipalIdentity::Human(_) => Ok(true),
            messaging::PrincipalIdentity::Agent(agent) => {
                let scope: Option<String> = sqlx::query_scalar(SELECT_AGENT_SESSION_SCOPE)
                    .bind(agent.as_str())
                    .fetch_optional(&mut *self.tx)
                    .await
                    .map_err(driver)?;
                Ok(scope.as_deref() == Some("agent"))
            }
        }
    }

    async fn endpoint_principal_exists(
        &mut self,
        endpoint: &ConversationEndpoint,
    ) -> Result<bool, PersistenceFailure> {
        let statement = match &endpoint.principal {
            messaging::PrincipalIdentity::Agent(_) => SELECT_AGENT_EXISTS,
            messaging::PrincipalIdentity::Human(_) => SELECT_USER_EXISTS,
        };
        let found: Option<i64> = sqlx::query_scalar(statement)
            .bind(endpoint.principal.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(found.is_some())
    }
}

impl<E: Send + Sync> PostAuthorityReads for TxScope<'_, '_, E> {
    async fn conversation_facts(
        &mut self,
        conversation: &ConversationId,
    ) -> Result<Option<ConversationFacts>, PersistenceFailure> {
        let row = sqlx::query(SELECT_CONVERSATION_FACTS)
            .bind(conversation.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        let Some(row) = row else { return Ok(None) };

        let closed_at: Option<i64> = row.try_get("closed_at").map_err(driver)?;
        let archived_at: Option<i64> = row.try_get("archived_at").map_err(driver)?;
        let placement: String = row.try_get("placement_kind").map_err(driver)?;

        let direct_pair = if placement == PLACEMENT_DIRECT_PAIR {
            Some(DirectPairEndpoints {
                a: endpoint_from_row(&row, EndpointSlot::A)?,
                b: endpoint_from_row(&row, EndpointSlot::B)?,
            })
        } else {
            None
        };

        Ok(Some(ConversationFacts {
            id: conversation.clone(),
            is_closed: closed_at.is_some(),
            is_archived: archived_at.is_some(),
            direct_pair,
        }))
    }

    async fn agent_exists(&mut self, agent: &AgentId) -> Result<bool, PersistenceFailure> {
        let found: Option<i64> = sqlx::query_scalar(SELECT_AGENT_EXISTS)
            .bind(agent.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(found.is_some())
    }

    async fn is_agent_participant(
        &mut self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        let found: Option<i64> = sqlx::query_scalar(SELECT_AGENT_PARTICIPATES)
            .bind(conversation.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(found.is_some())
    }

    async fn is_user_participant(
        &mut self,
        conversation: &ConversationId,
        user: &UserId,
    ) -> Result<bool, PersistenceFailure> {
        let found: Option<i64> = sqlx::query_scalar(SELECT_USER_PARTICIPATES)
            .bind(conversation.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(found.is_some())
    }
}

enum EndpointSlot {
    A,
    B,
}

fn endpoint_from_row(
    row: &sqlx::sqlite::SqliteRow,
    slot: EndpointSlot,
) -> Result<ConversationEndpoint, PersistenceFailure> {
    let malformed = || PersistenceFailure::from_source(MessagingStoreError::MalformedPlacement);
    let (kind_column, id_column, deleted_column) = match slot {
        EndpointSlot::A => ("endpoint_a_kind", "endpoint_a_id", "endpoint_a_deleted_at"),
        EndpointSlot::B => ("endpoint_b_kind", "endpoint_b_id", "endpoint_b_deleted_at"),
    };

    let kind: Option<String> = row.try_get(kind_column).map_err(driver)?;
    let id: Option<String> = row.try_get(id_column).map_err(driver)?;
    let deleted_at_ms: Option<i64> = row.try_get(deleted_column).map_err(driver)?;
    let (kind, id) = kind.zip(id).ok_or_else(malformed)?;

    let principal = match kind.as_str() {
        ENDPOINT_KIND_HUMAN => {
            messaging::PrincipalIdentity::Human(UserId::parse(id).map_err(|_| malformed())?)
        }
        ENDPOINT_KIND_AGENT => {
            messaging::PrincipalIdentity::Agent(AgentId::parse(id).map_err(|_| malformed())?)
        }
        _ => return Err(malformed()),
    };
    Ok(ConversationEndpoint {
        principal,
        deleted_at_ms,
    })
}

/// The durable snapshot shape, owned by the schema that holds it rather than by
/// the application type it is projected from.
#[derive(serde::Serialize)]
struct StoredAttachmentSnapshot<'a> {
    id: &'a str,
    filename: &'a str,
    size: i64,
    content_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

/// Attachments travel in the message's metadata column, so a message with none
/// stores no metadata at all rather than an empty container.
/// The send-time snapshot columns (`author_id`, `author_name`,
/// `author_display_name`) for an author, as the total projection of the value onto
/// what this schema stores. Active and deleted authors carry a named identity; a
/// system send (`None`) and an unresolved author name no one, so all three columns
/// are NULL. The write path only ever feeds an active author, but the projection is
/// defined for every arm so it cannot silently mishandle one.
fn author_snapshot_columns(
    author: Option<&MessageAuthor>,
) -> (Option<&str>, Option<&str>, Option<&str>) {
    match author {
        Some(MessageAuthor::Active(ActiveAuthor::Agent(agent))) => (
            Some(agent.id().as_str()),
            Some(agent.name()),
            Some(agent.display_name()),
        ),
        Some(MessageAuthor::Active(ActiveAuthor::Human(human))) => (
            Some(human.id().as_str()),
            Some(human.name()),
            Some(human.display_name()),
        ),
        Some(MessageAuthor::Deleted(deleted)) => (
            Some(deleted.identity().as_str()),
            Some(deleted.name()),
            Some(deleted.display_name()),
        ),
        Some(MessageAuthor::Unresolved(_)) | None => (None, None, None),
    }
}

fn metadata_json(attachments: &[AttachmentSnapshot]) -> Result<Option<String>, SendMessageError> {
    if attachments.is_empty() {
        return Ok(None);
    }
    let stored: Vec<StoredAttachmentSnapshot<'_>> = attachments
        .iter()
        .map(|snapshot| StoredAttachmentSnapshot {
            id: snapshot.id.as_str(),
            filename: &snapshot.filename,
            size: snapshot.size,
            content_type: &snapshot.content_type,
            label: snapshot.label.as_ref().map(AttachmentLabel::to_string),
        })
        .collect();

    serde_json::to_string(&serde_json::json!({ "attachments": stored }))
        .map(Some)
        .map_err(|error| infrastructure(MessagingStoreError::Serialization(error)))
}

// Canonical admitted request, not mutable resolved mentions or attachment snapshots.
// Preserve attachment order and normalized labels; destinations stay explicit.
fn client_request_json(command: &SendMessage) -> String {
    let destination = match command.destination() {
        SendDestination::MainLine => serde_json::json!(["main"]),
        SendDestination::Reply(id) => serde_json::json!(["reply", id.as_str()]),
        SendDestination::Thread(id) => serde_json::json!(["thread", id.as_str()]),
    };
    serde_json::json!({"conversation": command.target_conversation().as_str(),
        "destination": destination, "content": command.content().as_str(),
        "attachments": command.attachment_refs().iter().map(|a| serde_json::json!([a.id.as_str(), a.label])).collect::<Vec<_>>()}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Connection;
    use sqlx::sqlite::SqliteConnectOptions;
    use std::time::Instant;

    /// Opening needs a database file but no product schema, so these two facts
    /// about the connection itself are checked without one.
    async fn empty_database(path: &Path) {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        sqlx::SqlitePool::connect_with(options)
            .await
            .unwrap()
            .close()
            .await;
    }

    /// The clock's unit, asserted as a conversion: a wall-clock reading would make
    /// the assertion depend on the machine's time rather than on this code.
    #[test]
    fn the_host_clock_answers_in_unix_milliseconds() {
        let one_and_a_half_seconds = UNIX_EPOCH + Duration::from_millis(1_500);

        assert_eq!(unix_ms_since_epoch(UNIX_EPOCH).unwrap(), 0);
        assert_eq!(unix_ms_since_epoch(one_and_a_half_seconds).unwrap(), 1_500);
    }

    /// The host randomness is exercised for reachability only, alongside the host
    /// clock it shares an adapter with. What it produces is not asserted: any claim
    /// about the bytes would be a claim about chance.
    #[test]
    fn the_host_entropy_answers() {
        let mut buffer = [0u8; 32];

        assert!(HostEnvironment.fill(&mut buffer).is_ok());
    }

    /// A moment this system cannot express as a Unix millisecond is reported
    /// rather than truncated into a plausible one.
    #[test]
    fn a_moment_outside_the_supported_range_is_refused() {
        let before_epoch = UNIX_EPOCH - Duration::from_millis(1);
        let far_future = UNIX_EPOCH + Duration::from_secs(1 << 60);

        assert!(unix_ms_since_epoch(before_epoch).is_err());
        assert!(unix_ms_since_epoch(far_future).is_err());
    }

    /// The audience read must stay bounded by the cluster. A plan that scanned
    /// `agents` would make every send cost the size of the installation, which is
    /// exactly what the single statement exists to avoid.
    #[tokio::test]
    async fn the_audience_read_is_bounded_by_the_cluster_not_the_installation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.sqlite");
        let mut connection = crate::current_schema::create_current_schema(&path).await;

        let plan: Vec<String> = sqlx::query(EXPLAIN_CLUSTER_AUDIENCE)
            .bind("root-1")
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect();

        let followers_bounded = plan.iter().any(|step| {
            step.starts_with("SEARCH tf") || step.starts_with("SEARCH thread_followers")
        });
        assert!(
            followers_bounded,
            "the cluster's follower rows were not searched by root: {plan:?}"
        );
        for step in &plan {
            assert!(
                !step.starts_with("SCAN a") && !step.starts_with("SCAN agents"),
                "the plan scans every agent in the installation: {plan:?}"
            );
            assert!(
                !step.starts_with("SCAN tf"),
                "the plan scans every follower row: {plan:?}"
            );
        }
    }

    /// The entitlement read and the inserts are ONE write transaction, and the proof
    /// is that the read cannot happen without the writer.
    ///
    /// The batch chosen here would write NOTHING — it names somebody this database
    /// has never heard of — so an adapter that read on a snapshot and only asked for
    /// the writer when it had a row to insert would answer "nobody entitled" and
    /// succeed. Refusing instead is what says the decision and the rows it justifies
    /// share a transaction: nothing can be committed against a world that has moved
    /// on since the question was asked.
    #[tokio::test]
    async fn the_entitlement_decision_cannot_be_taken_without_the_writer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("entitlement-writer.sqlite");
        crate::current_schema::create_current_schema(&path)
            .await
            .close()
            .await
            .unwrap();
        let store = MessagingStore::open_with_busy_timeout(
            &path,
            HostEnvironment,
            Duration::from_millis(25),
        )
        .await
        .unwrap();

        let held = store.pool.begin_with(ROOT_WRITE_BEGIN).await.unwrap();
        let started = Instant::now();
        let failure = store
            .record_mentions_transaction(&MentionIntent::committed(
                vec![ParticipantHandle::parse("nobody").unwrap()],
                None,
                "Lio".into(),
                WorkspaceId::parse("ws-1").unwrap(),
                ConversationId::parse("conv-1").unwrap(),
                "Channel".into(),
                MessageId::parse("message-1").unwrap(),
                "named nobody".into(),
            ))
            .await
            .unwrap_err();

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "writer contention exceeded the controlled short timeout"
        );
        let MessagingStoreError::Sql(error) = &failure else {
            panic!("expected a driver failure, got {failure:?}");
        };
        assert_eq!(
            error
                .as_database_error()
                .and_then(|it| it.code())
                .as_deref(),
            Some("5"), // SQLITE_BUSY
        );
        held.rollback().await.unwrap();
    }

    /// The onboarding writer lands one complete invariant cluster: the row is a
    /// system message carrying only the onboarding subtype, it has no author and no
    /// thread root, it takes the conversation's next ordinal, and the main-line
    /// stream head advances to it. These are the same three facts every send writes,
    /// which is why provisioning reuses this writer rather than opening a second
    /// grammar for `(conversation, ordinal)` or for the stream head.
    #[tokio::test]
    async fn the_onboarding_writer_appends_one_complete_system_cluster() {
        use silverhand_backend_core::conversation_messaging::messaging::ParticipantHandle;
        use silverhand_backend_core::conversation_messaging::messaging::agent_onboarding::OnboardingSystemMessage;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("onboarding.sqlite");
        let mut conn = crate::current_schema::create_current_schema(&path).await;

        let conversation = ConversationId::parse("pair-1").unwrap();
        for statement in [
            "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 0)",
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1', \
             'human', 'Human', 0, 0)",
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
             created_by_user_id) VALUES ('agent-1', 'alice', 'claude', 'host-1', 0, 0, 'Alice', \
             'user-1')",
            "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
             endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, \
             updated_at) VALUES ('pair-1', 'direct_pair', 'fixed', 'DM', 'open', 'agent', \
             'agent-1', 'human', 'user-1', 0, 0)",
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }

        let agent_handle = ParticipantHandle::parse("Alice").unwrap();
        let creator_handle = ParticipantHandle::parse("shiki").unwrap();
        let intent = OnboardingSystemMessage::agent_direct_line(
            conversation.clone(),
            MessageId::parse("msg-onboard").unwrap(),
            &agent_handle,
            &creator_handle,
            2_000,
        );
        // The constructor renders the body; the row must store exactly what the intent holds.
        let expected_body = intent.content().to_owned();
        let witness = append_onboarding_system_message_on(&mut conn, &intent)
            .await
            .unwrap();

        let row = sqlx::query(
            "SELECT from_kind, metadata, seq, created_at, content, thread_root_id, from_agent_id, \
             from_user_id, author_id, author_name, author_display_name, cc_source_message_id \
             FROM messages WHERE id = ?",
        )
        .bind("msg-onboard")
        .fetch_one(&mut conn)
        .await
        .unwrap();

        assert_eq!(row.get::<String, _>("from_kind"), "system");
        assert_eq!(row.get::<String, _>("content"), expected_body);
        assert_eq!(row.get::<i64, _>("created_at"), 2_000);
        // First ordinal in a freshly minted line.
        assert_eq!(row.get::<i64, _>("seq"), 1);
        // The subtype is the whole of the metadata — no author or thread facts leak in.
        let metadata: serde_json::Value =
            serde_json::from_str(&row.get::<String, _>("metadata")).unwrap();
        assert_eq!(metadata, serde_json::json!({ "subtype": "onboarding" }));
        for null_column in [
            "thread_root_id",
            "from_agent_id",
            "from_user_id",
            "author_id",
            "author_name",
            "author_display_name",
            "cc_source_message_id",
        ] {
            assert_eq!(
                row.get::<Option<String>, _>(null_column),
                None,
                "a system onboarding message carries no {null_column}"
            );
        }

        // The main line's stream head (root IS NULL) advanced to the message.
        let head: i64 = sqlx::query_scalar(
            "SELECT latest_seq FROM conversation_stream_heads WHERE conversation_id = ? \
             AND thread_root_id IS NULL",
        )
        .bind(conversation.as_str())
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(head, 1);

        // The returned witness is the row's own after-image — every column this writer
        // touched, read back — so the domain can bind the whole write against the intent.
        // Sealing itself is core's (`seal_written` is `pub(crate)`); this proves the adapter
        // hands back the faithful shape the seal will consume.
        assert_eq!(witness.id, "msg-onboard");
        assert_eq!(witness.conversation, "pair-1");
        assert_eq!(witness.content, expected_body);
        assert_eq!(witness.created_at_ms, 2_000);
        assert_eq!(witness.from_kind, "system");
        assert_eq!(witness.from_agent_id, None);
        assert_eq!(witness.from_user_id, None);
        assert_eq!(witness.author_id, None);
        assert_eq!(witness.author_name, None);
        assert_eq!(witness.author_display_name, None);
        assert_eq!(witness.thread_root_id, None);
        assert_eq!(witness.cc_source_message_id, None);
        assert_eq!(witness.metadata.subtype.as_deref(), Some("onboarding"));
        assert!(!witness.metadata.has_extra_keys);
        assert_eq!(witness.seq, 1);
        assert_eq!(witness.head_latest_seq, 1);
    }

    /// Seed the graph the onboarding inbox-ensure witnesses run against: a workspace
    /// conversation with `agent-ws` on its roster, a direct-pair line with `agent-dp` as an
    /// endpoint, a non-participant `agent-out`, and four messages — a genuine onboarding system
    /// message in each conversation, plus (in the workspace line) an ordinary agent message and
    /// a system message whose subtype is NOT onboarding. Returns the store built over the same
    /// database so a test can exercise the real `onboarding_inbox_ensure()` builder path.
    async fn seed_onboarding_ensure_graph(path: &std::path::Path) -> MessagingStore {
        let mut conn = crate::current_schema::create_current_schema(path).await;
        for statement in [
            "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 0)",
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1', \
             'human', 'Human', 0, 0)",
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
             created_by_user_id) VALUES ('agent-ws', 'wsagent', 'claude', 'host-1', 0, 0, 'Ws', \
             'user-1')",
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
             created_by_user_id) VALUES ('agent-dp', 'dpagent', 'claude', 'host-1', 0, 0, 'Dp', \
             'user-1')",
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
             created_by_user_id) VALUES ('agent-out', 'outagent', 'claude', 'host-1', 0, 0, 'Out', \
             'user-1')",
            "INSERT INTO workspaces (id, name, name_key, created_at, updated_at) VALUES \
             ('workspace-1', 'Space', 'space', 0, 0)",
            "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
             title_key, status, visibility, created_at, updated_at, latest_message_seq) VALUES \
             ('conv-ws', 'workspace-1', 'workspace', 'open', 'Chan', 'chan', 'open', 'private', 0, \
             0, 3)",
            "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
             endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, \
             updated_at, latest_message_seq) VALUES ('conv-dp', 'direct_pair', 'fixed', 'DM', \
             'open', 'agent', 'agent-dp', 'human', 'user-1', 0, 0, 1)",
            "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES \
             ('conv-ws', 'agent-ws', 0)",
            // A genuine onboarding system message in each line: system from-kind, metadata that
            // is EXACTLY the onboarding subtype.
            "INSERT INTO messages (id, conversation_id, from_kind, content, metadata, seq, \
             created_at) VALUES ('msg-ws', 'conv-ws', 'system', 'hi', '{\"subtype\":\"onboarding\"}', \
             1, 0)",
            "INSERT INTO messages (id, conversation_id, from_kind, content, metadata, seq, \
             created_at) VALUES ('msg-dp', 'conv-dp', 'system', 'hi', '{\"subtype\":\"onboarding\"}', \
             1, 0)",
            // An ORDINARY agent message a participant sent into the same line — the shape witness
            // must refuse it even though message-in-conversation and participation both hold.
            "INSERT INTO messages (id, conversation_id, from_kind, from_agent_id, content, seq, \
             created_at) VALUES ('msg-ordinary', 'conv-ws', 'agent', 'agent-ws', 'chatter', 2, 0)",
            // A system message carrying a DIFFERENT subtype — right from-kind, wrong metadata.
            "INSERT INTO messages (id, conversation_id, from_kind, content, metadata, seq, \
             created_at) VALUES ('msg-wrongsub', 'conv-ws', 'system', 'x', '{\"subtype\":\"reply\"}', \
             3, 0)",
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }
        drop(conn);
        MessagingStore::open(path).await.unwrap()
    }

    async fn inbox_row_count(store: &MessagingStore) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM conversation_inboxes")
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    fn conv(raw: &str) -> ConversationId {
        ConversationId::parse(raw).unwrap()
    }
    fn agent(raw: &str) -> AgentId {
        AgentId::parse(raw).unwrap()
    }
    fn msg(raw: &str) -> MessageId {
        MessageId::parse(raw).unwrap()
    }

    /// The happy path across BOTH placement arms: an onboarding message plus a genuine
    /// participant — a workspace roster row in one line, a direct-pair endpoint in the other —
    /// each enqueues exactly one undelivered turn. This is what proves the ONE participation
    /// predicate is really being consumed on both arms, not just the workspace one.
    #[tokio::test]
    async fn the_inbox_ensure_enqueues_a_turn_for_a_participant_of_either_placement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ensure-happy.sqlite");
        let store = seed_onboarding_ensure_graph(&path).await;
        let ensure = store.onboarding_inbox_ensure();

        ensure
            .ensure_inner(&conv("conv-ws"), &agent("agent-ws"), &msg("msg-ws"))
            .await
            .expect("a roster participant's onboarding message enqueues");
        ensure
            .ensure_inner(&conv("conv-dp"), &agent("agent-dp"), &msg("msg-dp"))
            .await
            .expect("a direct-pair endpoint's onboarding message enqueues");

        // Two undelivered turns, one per line, bound to the onboarding message.
        let rows: Vec<(String, String, String, Option<i64>)> = sqlx::query_as(
            "SELECT conversation_id, agent_id, message_id, delivered_at FROM conversation_inboxes \
             ORDER BY conversation_id",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "conv-dp".to_owned(),
                    "agent-dp".to_owned(),
                    "msg-dp".to_owned(),
                    None
                ),
                (
                    "conv-ws".to_owned(),
                    "agent-ws".to_owned(),
                    "msg-ws".to_owned(),
                    None
                ),
            ]
        );
    }

    #[tokio::test]
    async fn committed_system_message_ensure_reuses_the_same_custody_writer_without_onboarding_gate()
     {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ensure-committed.sqlite");
        let store = seed_onboarding_ensure_graph(&path).await;
        let ensure = store.committed_message_inbox_ensure();

        ensure
            .ensure_for_live_recipient(&conv("conv-ws"), &agent("agent-ws"), &msg("msg-wrongsub"))
            .await
            .unwrap();
        ensure
            .ensure_for_live_recipient(&conv("conv-ws"), &agent("agent-ws"), &msg("msg-wrongsub"))
            .await
            .unwrap();
        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT conversation_id,agent_id,message_id FROM conversation_inboxes")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![(
                "conv-ws".to_owned(),
                "agent-ws".to_owned(),
                "msg-wrongsub".to_owned(),
            )],
            "the generic committed-message authority is idempotent and preserves the exact message coordinate"
        );
    }

    #[tokio::test]
    async fn live_and_sealed_recipient_authorities_diverge_after_membership_revoke() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ensure-authority.sqlite");
        let store = seed_onboarding_ensure_graph(&path).await;
        let ensure = store.committed_message_inbox_ensure();
        sqlx::query(
            "DELETE FROM conversation_agents WHERE conversation_id='conv-ws' AND agent_id='agent-ws'",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        ensure
            .ensure_for_live_recipient(&conv("conv-ws"), &agent("agent-ws"), &msg("msg-wrongsub"))
            .await
            .unwrap_err();
        assert_eq!(inbox_row_count(&store).await, 0);

        ensure
            .ensure_for_sealed_recipient(&conv("conv-ws"), &agent("agent-ws"), &msg("msg-wrongsub"))
            .await
            .unwrap();
        assert_eq!(
            inbox_row_count(&store).await,
            1,
            "a writer-sealed recipient remains deliverable after the commit boundary"
        );
    }

    /// A message that is not in the named conversation — and an id that names no message at all
    /// — fault as `Message` and write nothing. The route's supplied ids are treated as a claim
    /// to be witnessed, never as a relationship.
    #[tokio::test]
    async fn an_ensure_for_a_message_outside_the_conversation_faults_and_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ensure-wrong-conv.sqlite");
        let store = seed_onboarding_ensure_graph(&path).await;
        let ensure = store.onboarding_inbox_ensure();

        // `msg-ws` is a real onboarding message, but it lives in conv-ws, not conv-dp.
        let cross = ensure
            .ensure_inner(&conv("conv-dp"), &agent("agent-dp"), &msg("msg-ws"))
            .await
            .unwrap_err();
        assert!(matches!(
            cross,
            OnboardingInboxEnsureError::WitnessMismatch(DeliveryWitnessMismatch::Message)
        ));

        // An id that names no row at all.
        let unknown = ensure
            .ensure_inner(&conv("conv-ws"), &agent("agent-ws"), &msg("msg-nope"))
            .await
            .unwrap_err();
        assert!(matches!(
            unknown,
            OnboardingInboxEnsureError::WitnessMismatch(DeliveryWitnessMismatch::Message)
        ));

        assert_eq!(inbox_row_count(&store).await, 0);
    }

    /// An agent that is not a participant of the conversation faults as `Participation` and
    /// writes nothing, even for a genuine onboarding message.
    #[tokio::test]
    async fn an_ensure_for_a_nonparticipant_faults_and_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ensure-nonparticipant.sqlite");
        let store = seed_onboarding_ensure_graph(&path).await;
        let ensure = store.onboarding_inbox_ensure();

        let fault = ensure
            .ensure_inner(&conv("conv-ws"), &agent("agent-out"), &msg("msg-ws"))
            .await
            .unwrap_err();
        assert!(matches!(
            fault,
            OnboardingInboxEnsureError::WitnessMismatch(DeliveryWitnessMismatch::Participation)
        ));
        assert_eq!(inbox_row_count(&store).await, 0);
    }

    /// The core of the P0 fix: a message that is in the conversation and whose agent
    /// participates, but that is NOT an onboarding message — an ordinary agent message, or a
    /// system message with the wrong subtype — is refused as `OnboardingShape` and writes
    /// nothing. Without the shape witness this narrow door would be a generic delivery writer.
    #[tokio::test]
    async fn an_ensure_for_an_ordinary_or_wrong_subtype_message_is_refused_as_wrong_shape() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ensure-wrong-shape.sqlite");
        let store = seed_onboarding_ensure_graph(&path).await;
        let ensure = store.onboarding_inbox_ensure();

        for message in ["msg-ordinary", "msg-wrongsub"] {
            let fault = ensure
                .ensure_inner(&conv("conv-ws"), &agent("agent-ws"), &msg(message))
                .await
                .unwrap_err();
            assert!(
                matches!(
                    fault,
                    OnboardingInboxEnsureError::WitnessMismatch(
                        DeliveryWitnessMismatch::OnboardingShape { .. }
                    )
                ),
                "{message} should be refused as the wrong shape, got {fault:?}"
            );
        }
        assert_eq!(inbox_row_count(&store).await, 0);
    }

    /// The ensure is idempotent: re-running it over a row that already exists conflicts on the
    /// unique delivery index and DOES NOTHING — it never resets the delivered / failed / lease
    /// columns a drain or a failure has since written. A re-driven create leg cannot resurrect a
    /// completed delivery.
    #[tokio::test]
    async fn an_ensure_over_an_existing_row_leaves_its_delivery_columns_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ensure-idempotent.sqlite");
        let store = seed_onboarding_ensure_graph(&path).await;

        // A prior delivery already ran: the row is delivered, once leased, and now failed.
        sqlx::query(
            "INSERT INTO conversation_inboxes (conversation_id, agent_id, message_id, \
             delivered_at, failed_at, in_flight_owner, delivery_kind, created_at) VALUES \
             ('conv-ws', 'agent-ws', 'msg-ws', 555, 777, 'daemon-9', 'turn', 111)",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        store
            .onboarding_inbox_ensure()
            .ensure_inner(&conv("conv-ws"), &agent("agent-ws"), &msg("msg-ws"))
            .await
            .expect("a witnessed ensure over an existing row succeeds");

        // Still exactly one row, and every delivery-state column is exactly as the prior
        // delivery left it — the ON CONFLICT DO NOTHING wrote nothing new.
        let row: (i64, Option<i64>, Option<i64>, Option<String>, String, i64) = sqlx::query_as(
            "SELECT count(*) OVER (), delivered_at, failed_at, in_flight_owner, delivery_kind, \
             created_at FROM conversation_inboxes WHERE conversation_id = 'conv-ws' AND agent_id = \
             'agent-ws' AND message_id = 'msg-ws'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(row.0, 1, "the ensure created a duplicate row");
        assert_eq!(row.1, Some(555), "delivered_at was reset");
        assert_eq!(row.2, Some(777), "failed_at was reset");
        assert_eq!(
            row.3.as_deref(),
            Some("daemon-9"),
            "the lease owner was reset"
        );
        assert_eq!(row.4, "turn");
        assert_eq!(row.5, 111, "created_at was overwritten");
    }

    /// The entitlement read must stay bounded by the NAMES A BODY USED. It runs while
    /// the adapter holds SQLite's single writer, so a plan that scanned `users` — or
    /// either membership table — would make one mention cost the size of the
    /// installation and hold the writer for it.
    #[tokio::test]
    async fn the_entitlement_read_is_bounded_by_the_named_set_not_the_installation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("entitlement-plan.sqlite");
        let mut connection = crate::current_schema::create_current_schema(&path).await;

        let plan: Vec<String> = sqlx::query(EXPLAIN_ENTITLED_VIEWERS)
            .bind(r#"["human"]"#)
            .bind(Option::<&str>::None)
            .bind("conv-1")
            .bind("ws-1")
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect();

        // What has to hold is the COST, not one spelling of it: `users` is reached by
        // NAME, so the read is bounded by the names the body used. Reaching it any
        // other way makes one mention cost the installation — that is the shape the
        // prohibitions below forbid, and it is what a correlated rewrite produces.
        // The `ORDER BY` temp b-tree sorts that same bounded set rather than a table.
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH u") || step.starts_with("SEARCH users")),
            "a named handle was not resolved through an index on `users`: {plan:?}"
        );
        for step in &plan {
            assert!(
                !step.starts_with("SCAN u") && !step.starts_with("SCAN users"),
                "the plan scans every person in the installation: {plan:?}"
            );
            assert!(
                !step.starts_with("SCAN cu")
                    && !step.starts_with("SCAN conversation_users")
                    && !step.starts_with("SCAN wu")
                    && !step.starts_with("SCAN workspace_users"),
                "the plan scans a membership table instead of probing it: {plan:?}"
            );
        }
    }

    /// The batch author lookups must be bounded by the ids asked for, not by the
    /// installation. Each is driven by `json_each` over the requested set and probes
    /// its table's primary key, so a set of any size costs one indexed read per row
    /// asked for — never a scan of `agents`/`users`. The write path's point read is
    /// this same statement with a one-element set, so the bound holds there too.
    #[tokio::test]
    async fn the_batch_author_lookups_are_bounded_by_the_requested_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("author-plan.sqlite");
        let mut connection = crate::current_schema::create_current_schema(&path).await;

        let agents_plan: Vec<String> = sqlx::query(author::EXPLAIN_LIVE_AGENTS)
            .bind(r#"["agent-1","agent-2"]"#)
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect();
        assert!(
            agents_plan
                .iter()
                .any(|step| step.starts_with("SEARCH a") || step.starts_with("SEARCH agents")),
            "an author agent id was not resolved through the agents primary key: {agents_plan:?}"
        );
        for step in &agents_plan {
            assert!(
                !step.starts_with("SCAN a") && !step.starts_with("SCAN agents"),
                "the plan scans every agent in the installation: {agents_plan:?}"
            );
        }

        let users_plan: Vec<String> = sqlx::query(author::EXPLAIN_LIVE_USERS)
            .bind(r#"["user-1","user-2"]"#)
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect();
        assert!(
            users_plan
                .iter()
                .any(|step| step.starts_with("SEARCH u") || step.starts_with("SEARCH users")),
            "an author user id was not resolved through the users primary key: {users_plan:?}"
        );
        for step in &users_plan {
            assert!(
                !step.starts_with("SCAN u") && !step.starts_with("SCAN users"),
                "the plan scans every user in the installation: {users_plan:?}"
            );
        }
    }

    /// A mixed page resolves every arm through the ONE author authority in a single
    /// call, in input order: the live agents and human come from the two set-shaped
    /// reads that authority runs (one per table), and the deleted and unresolved rows
    /// from their own evidence with no read, because a nulled FK asks for nothing.
    /// Two agents resolving from one call is the no-N+1 shape — the batch's live
    /// actors cost one read per table, whatever the page size — and the caller never
    /// touches the loader or the discriminator directly, only `resolve_authors`.
    #[tokio::test]
    async fn a_mixed_page_resolves_every_arm_through_one_batch_authority() {
        use silverhand_backend_core::{
            conversation_messaging::messaging::PrincipalIdentity,
            identity_access::agents::runtime::RuntimeKind,
        };

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mixed-batch.sqlite");
        let mut db = crate::current_schema::create_current_schema(&path).await;

        for statement in [
            "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 0)",
            "INSERT INTO users (id, name, display_name, avatar_storage_key, avatar_content_type, \
             created_at, updated_at) VALUES ('user-1', 'human', 'Human', 'avatars/u.png', \
             'image/png', 0, 7)",
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
             created_by_user_id) VALUES ('agent-1', 'lio', 'claude', 'host-1', 0, 0, 'Lio', \
             'user-1')",
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
             created_by_user_id) VALUES ('agent-2', 'cin', 'codex', 'host-1', 0, 0, 'Cin', \
             'user-1')",
        ] {
            sqlx::query(statement).execute(&mut db).await.unwrap();
        }

        let agent_fk = |fk: &str| AuthorEvidence {
            from_kind: FromKind::Agent,
            from_agent_id: Some(fk.to_owned()),
            from_user_id: None,
            author_id: None,
            author_name: None,
            author_display_name: None,
        };

        // One call, five rows, mixed arms — the order the batch is asked in is the
        // order it answers in.
        let evidence = vec![
            agent_fk("agent-1"),
            AuthorEvidence {
                from_kind: FromKind::User,
                from_agent_id: None,
                from_user_id: Some("user-1".to_owned()),
                author_id: None,
                author_name: None,
                author_display_name: None,
            },
            // Deleted: FK nulled by the delete, its send-time snapshot surviving.
            AuthorEvidence {
                from_kind: FromKind::Agent,
                from_agent_id: None,
                from_user_id: None,
                author_id: Some("agent-gone".to_owned()),
                author_name: Some("gone".to_owned()),
                author_display_name: Some("Gone".to_owned()),
            },
            // Unresolved: FK nulled and no snapshot ever recorded.
            AuthorEvidence {
                from_kind: FromKind::Agent,
                from_agent_id: None,
                from_user_id: None,
                author_id: None,
                author_name: None,
                author_display_name: None,
            },
            agent_fk("agent-2"),
        ];

        let resolved = resolve_authors(&mut db, &evidence).await.unwrap();
        assert_eq!(resolved.len(), 5, "one answer per input row, in order");

        // [0] live agent with its runtime.
        match resolved[0].as_ref().expect("an agent send names an author") {
            MessageAuthor::Active(ActiveAuthor::Agent(agent)) => {
                assert_eq!(agent.name(), "lio");
                assert_eq!(agent.display_name(), "Lio");
                assert_eq!(agent.runtime(), RuntimeKind::Claude);
                assert_eq!(agent.avatar(), None);
            }
            other => panic!("expected a live agent at [0], got {other:?}"),
        }

        // [1] live human: no runtime, an avatar revision from the row's updated_at.
        match resolved[1].as_ref().expect("a human send names an author") {
            MessageAuthor::Active(ActiveAuthor::Human(person)) => {
                assert_eq!(person.name(), "human");
                assert_eq!(person.avatar().map(|a| a.cache_buster()), Some(7));
            }
            other => panic!("expected a live human at [1], got {other:?}"),
        }

        // [2] deleted: named by its snapshot, no live row requested for it.
        match resolved[2]
            .as_ref()
            .expect("a deleted author is named by its snapshot")
        {
            MessageAuthor::Deleted(author) => {
                assert_eq!(author.display_name(), "Gone");
                assert!(matches!(author.identity(), PrincipalIdentity::Agent(_)));
            }
            other => panic!("expected a deleted author at [2], got {other:?}"),
        }

        // [3] unresolved: named by nothing.
        assert!(matches!(
            resolved[3]
                .as_ref()
                .expect("an unresolved author still reports its kind"),
            MessageAuthor::Unresolved(_)
        ));

        // [4] the second live agent, resolved from the SAME agents read as [0].
        match resolved[4].as_ref().expect("an agent send names an author") {
            MessageAuthor::Active(ActiveAuthor::Agent(agent)) => {
                assert_eq!(agent.name(), "cin");
                assert_eq!(agent.runtime(), RuntimeKind::Codex);
            }
            other => panic!("expected a live agent at [4], got {other:?}"),
        }
    }

    /// A single broken row fails the WHOLE batch — there is no partial page of
    /// authors, and the fault the store owns (a stored runtime outside the closed
    /// set) survives as the error's source. The valid rows around it do not soften
    /// the invariant into a per-row skip.
    #[tokio::test]
    async fn one_broken_runtime_fails_the_whole_batch_with_its_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broken-runtime-batch.sqlite");
        let mut db = crate::current_schema::create_current_schema(&path).await;

        for statement in [
            "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 0)",
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1', \
             'human', 'Human', 0, 0)",
            // A stored runtime the domain does not accept — the DB has no CHECK on the
            // value, so this is exactly the data-invariant fault the store must own.
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
             created_by_user_id) VALUES ('agent-bad', 'bad', 'aider', 'host-1', 0, 0, 'Bad', \
             'user-1')",
        ] {
            sqlx::query(statement).execute(&mut db).await.unwrap();
        }

        let evidence = vec![
            AuthorEvidence {
                from_kind: FromKind::User,
                from_agent_id: None,
                from_user_id: Some("user-1".to_owned()),
                author_id: None,
                author_name: None,
                author_display_name: None,
            },
            AuthorEvidence {
                from_kind: FromKind::Agent,
                from_agent_id: Some("agent-bad".to_owned()),
                from_user_id: None,
                author_id: None,
                author_name: None,
                author_display_name: None,
            },
        ];

        let failure = resolve_authors(&mut db, &evidence).await.unwrap_err();
        let source = std::error::Error::source(&failure)
            .and_then(|error| error.downcast_ref::<MessagingStoreError>())
            .expect("the persistence failure carries the store error as its source");
        assert!(
            matches!(source, MessagingStoreError::MalformedRuntime { value } if value == "aider"),
            "the broken runtime value was not preserved through the batch failure: {source:?}"
        );
    }

    /// The one cell the send path cannot reach: a follower row that names an agent
    /// whose row is gone, on a placement whose participation is named by the
    /// conversation row rather than by a roster with a foreign key. Participation
    /// says yes and existence says no, and the two texts have to agree that the
    /// answer is no — a delivery row for a principal that does not exist cannot be
    /// written at all. Reached directly because a survivor's further sends into such
    /// a pair are refused, so no send can observe it.
    #[tokio::test]
    async fn the_two_participation_texts_agree_when_a_follower_no_longer_exists() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("liveness.sqlite");
        let mut db = crate::current_schema::create_current_schema(&path).await;

        for statement in [
            "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 0)",
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1', \
             'human', 'Human', 0, 0)",
            "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
             display_name, created_by_user_id) VALUES ('agent-gone', 'gone', 'claude', 'host-1', \
             0, 0, 'Gone', 'user-1')",
            "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
             endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, \
             updated_at) VALUES ('pair-1', 'direct_pair', 'fixed', 'DM', 'open', 'agent', \
             'agent-gone', 'human', 'user-1', 0, 0)",
            "INSERT INTO messages (id, conversation_id, from_kind, content, seq, created_at) \
             VALUES ('root-000000000000', 'pair-1', 'system', 'anchor', 1, 0)",
            "INSERT INTO thread_followers (thread_root_id, conversation_id, principal_kind, \
             principal_id, followed_at, source) VALUES ('root-000000000000', 'pair-1', 'agent', \
             'agent-gone', 1, 'reply')",
        ] {
            sqlx::query(statement).execute(&mut db).await.unwrap();
        }

        let audience_before: Vec<String> = sqlx::query_scalar(SELECT_CLUSTER_AUDIENCE)
            .bind("root-000000000000")
            .fetch_all(&mut db)
            .await
            .unwrap();
        assert_eq!(audience_before, vec!["agent-gone".to_owned()]);

        // The agents row goes; the pair still names the same id as its endpoint,
        // because that column is not a foreign key.
        sqlx::query("DELETE FROM agents WHERE id = 'agent-gone'")
            .execute(&mut db)
            .await
            .unwrap();

        let participates: Option<i64> = sqlx::query_scalar(SELECT_AGENT_PARTICIPATES)
            .bind("pair-1")
            .bind("agent-gone")
            .bind("agent-gone")
            .bind("agent-gone")
            .fetch_optional(&mut db)
            .await
            .unwrap();
        let exists: Option<i64> = sqlx::query_scalar(SELECT_AGENT_EXISTS)
            .bind("agent-gone")
            .fetch_optional(&mut db)
            .await
            .unwrap();
        let audience_after: Vec<String> = sqlx::query_scalar(SELECT_CLUSTER_AUDIENCE)
            .bind("root-000000000000")
            .fetch_all(&mut db)
            .await
            .unwrap();

        assert!(
            participates.is_some(),
            "the pair stopped naming its endpoint"
        );
        assert!(exists.is_none());
        assert!(
            audience_after.is_empty(),
            "the audience kept an agent that no longer exists"
        );
    }

    #[tokio::test]
    async fn the_store_refuses_to_provision_a_database_it_does_not_own() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("absent.db");

        let outcome = MessagingStore::open(&path).await;

        assert!(matches!(outcome, Err(MessagingStoreError::Sql(_))));
        assert!(!path.exists(), "the store created a database file");
    }

    #[tokio::test]
    async fn every_pool_connection_carries_the_durability_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("configuration.db");
        empty_database(&path).await;
        let store = MessagingStore::open(&path).await.unwrap();

        let mut connections = Vec::new();
        for _ in 0..POOL_CONNECTIONS {
            connections.push(store.pool.acquire().await.unwrap());
        }
        for connection in &mut connections {
            // Inspect the linked library, not the Cargo environment: a dependency
            // upgrade must not silently restore the global cache LRU extension.
            let options: Vec<String> = sqlx::query_scalar("PRAGMA compile_options")
                .fetch_all(&mut **connection)
                .await
                .unwrap();
            assert!(!options.iter().any(|o| o == "ENABLE_MEMORY_MANAGEMENT"));
            assert!(options.iter().any(|o| o == "THREADSAFE=1"));
            assert!(!options.iter().any(|o| o == "DEFAULT_MEMSTATUS=0"));
            let cache_size: i64 = sqlx::query_scalar("PRAGMA cache_size")
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            assert_eq!(cache_size, -2000);
            let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            let busy_timeout_ms: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
                .fetch_one(&mut **connection)
                .await
                .unwrap();

            assert_eq!(journal_mode, "wal");
            assert_eq!(synchronous, 2); // FULL
            assert_eq!(foreign_keys, 1);
            assert_eq!(busy_timeout_ms, 5_000);
        }
    }

    /// The readiness probe this store mints IS this store's handle.
    ///
    /// Closing the store closes the probe with it. A probe that opened its own pool on
    /// the same file would still answer `Ok` after this point — a green `/readyz` for
    /// a process whose writer is gone — so the shared handle is what the gate is
    /// about, and it cannot be shown by reading the wiring.
    #[tokio::test]
    async fn the_probe_this_store_mints_dies_with_the_store() {
        use silverhand_backend_core::foundation::persistence::WriteProbe;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("readiness-handle.db");
        empty_database(&path).await;
        let store = MessagingStore::open(&path).await.unwrap();
        let probe = store.write_probe();

        probe
            .probe_write()
            .await
            .expect("the store's database is writable");

        store.pool.close().await;

        probe
            .probe_write()
            .await
            .expect_err("a closed store is not a writable one");
    }
}

mod profile_message_counts;
