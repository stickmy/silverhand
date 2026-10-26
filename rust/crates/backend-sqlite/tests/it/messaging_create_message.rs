//! The create-message write unit against the real product schema.
//!
//! Each test builds its own database from the checked-in executable schema
//! artifact under the system temp root; no live database is opened, and the store
//! under test provisions nothing.

use silverhand_backend_core::conversation_messaging::messaging::ParticipantHandle;
use silverhand_backend_core::conversation_messaging::messaging::mention_cc::{
    CommittedMentionCopy, MentionCopyOutcome, MentionCopySource, render_copy_body,
};
use silverhand_backend_core::conversation_messaging::messaging::post_commit::{
    CommittedMentionRow, CommittedMessagePublication, CopyNotification, DirectLineInvalidation,
    EffectFailure, EffectStep, IdleStart, MainDelivery, MainEffects, MentionSkipRecord,
    MessagePublication, ReplyDelivery, ReplyEffects, RuntimeDelivery, RuntimeStart, SendEffects,
    SkippedMention, UnreadPublication, ViewerNotification, announce_main_send, announce_reply_send,
    announce_send,
};
use silverhand_backend_core::conversation_messaging::messaging::{
    ActiveAuthor, ActorRefusal, AddressedPrincipals, AttachmentId, ConversationRefusal,
    DirectLineAuthority, EnvironmentUnavailable, FromKind, MAX_MESSAGE_CONTENT_BYTES,
    MessageAuthor, MessageContent, MessageId, MessageSeq, PrincipalKind, ReferencePositions,
    ReplyTargetRefusal, SendActor, SendClock, SendDestination, SendEntropy, SendMessage,
    SendMessageDraft, SendMessageError, SendMessageUnitOfWork, THREAD_PREVIEW_MAX_UTF16,
    ThreadListEntry, ThreadRootId, ThreadRootRefusal, attachments::AttachmentRef,
    thread_root_preview,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use silverhand_backend_core::identity_access::agents::runtime::RuntimeKind;
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    HostEnvironment, MessagingStore,
};
use sqlx::{Row, SqliteConnection};
use std::{
    error::Error,
    future::Future,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};
use tempfile::TempDir;

use crate::current_schema;

const HOST_ID: &str = "host-1";
const USER_ID: &str = "user-1";
const AGENT_ID: &str = "agent-1";
/// A second runtime, declared at birth to judge freshness across every
/// conversation. The scope is immutable, so the broad arm needs its own agent.
const BROAD_AGENT_ID: &str = "agent-2";
/// Known to the installation but a participant of nothing, so following it would
/// have to be a way into a conversation.
const OUTSIDER_AGENT_ID: &str = "agent-3";
const WORKSPACE_ID: &str = "ws-1";
const CHANNEL_ID: &str = "conv-1";
const OTHER_CHANNEL_ID: &str = "conv-2";
const PAIR_ID: &str = "pair-1";

struct Fixture {
    _directory: TempDir,
    path: PathBuf,
    db: SqliteConnection,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_scope("conversation").await
    }

    async fn with_scope(scope: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("product.sqlite");
        let mut db = current_schema::create_current_schema(&path).await;
        seed(&mut db, scope).await;
        Self {
            _directory: directory,
            path,
            db,
        }
    }

    async fn store(&self) -> MessagingStore {
        MessagingStore::open(&self.path).await.unwrap()
    }

    async fn store_with<E>(&self, environment: E) -> MessagingStore<E> {
        MessagingStore::open_with(&self.path, environment)
            .await
            .unwrap()
    }

    async fn count(&mut self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(&mut self.db)
            .await
            .unwrap()
    }

    async fn latest_seq(&mut self, conversation: &str) -> i64 {
        sqlx::query_scalar("SELECT latest_message_seq FROM conversations WHERE id = ?")
            .bind(conversation)
            .fetch_one(&mut self.db)
            .await
            .unwrap()
    }

    async fn stream_head(&mut self, conversation: &str, thread_root: Option<&str>) -> Option<i64> {
        sqlx::query_scalar(
            "SELECT latest_seq FROM conversation_stream_heads WHERE conversation_id = ? AND \
             thread_root_id IS ?",
        )
        .bind(conversation)
        .bind(thread_root)
        .fetch_optional(&mut self.db)
        .await
        .unwrap()
    }

    async fn execute(&mut self, sql: &'static str) {
        sqlx::query(sql).execute(&mut self.db).await.unwrap();
    }

    async fn followers(&mut self, root: &str) -> Vec<(String, String, String)> {
        sqlx::query(
            "SELECT principal_kind, principal_id, source FROM thread_followers WHERE \
             thread_root_id = ? ORDER BY principal_kind ASC, principal_id ASC",
        )
        .bind(root)
        .fetch_all(&mut self.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("principal_kind"),
                row.get::<String, _>("principal_id"),
                row.get::<String, _>("source"),
            )
        })
        .collect()
    }

    async fn queued_for(&mut self, agent: &str) -> Vec<(String, String)> {
        sqlx::query(
            "SELECT message_id, delivery_kind FROM conversation_inboxes WHERE agent_id = ? ORDER \
             BY id ASC",
        )
        .bind(agent)
        .fetch_all(&mut self.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("message_id"),
                row.get::<String, _>("delivery_kind"),
            )
        })
        .collect()
    }

    /// The full delivery witness for an agent, settlement columns included, so a
    /// test can assert the exact row a send produced rather than only its message id.
    async fn witness_rows(&mut self, agent: &str) -> Vec<InboxWitness> {
        sqlx::query(
            "SELECT conversation_id, message_id, delivery_kind, delivered_at, failed_at, \
             in_flight_owner FROM conversation_inboxes WHERE agent_id = ? ORDER BY id ASC",
        )
        .bind(agent)
        .fetch_all(&mut self.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| InboxWitness {
            conversation_id: row.get::<String, _>("conversation_id"),
            message_id: row.get::<String, _>("message_id"),
            delivery_kind: row.get::<String, _>("delivery_kind"),
            delivered_at: row.get::<Option<i64>, _>("delivered_at"),
            failed_at: row.get::<Option<i64>, _>("failed_at"),
            in_flight_owner: row.get::<Option<String>, _>("in_flight_owner"),
        })
        .collect()
    }
}

/// A `conversation_inboxes` row read whole, so the settlement columns a pending
/// `turn` must leave empty are part of the assertion, not assumed.
#[derive(Debug, PartialEq, Eq)]
struct InboxWitness {
    conversation_id: String,
    message_id: String,
    delivery_kind: String,
    delivered_at: Option<i64>,
    failed_at: Option<i64>,
    in_flight_owner: Option<String>,
}

/// The Messaging seed: the smallest set of rows the send path reads, expressed
/// against the real constraints rather than around them.
async fn seed(db: &mut SqliteConnection, scope: &str) {
    sqlx::query("INSERT INTO hosts (id, name, kind, created_at) VALUES (?, 'host', 'local', 0)")
        .bind(HOST_ID)
        .execute(&mut *db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES (?, 'human', \
         'Human', 0, 0)",
    )
    .bind(USER_ID)
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
         created_by_user_id, session_scope) VALUES (?, 'lio', 'claude', ?, 0, 0, 'Lio', ?, ?)",
    )
    .bind(AGENT_ID)
    .bind(HOST_ID)
    .bind(USER_ID)
    .bind(scope)
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
         created_by_user_id, session_scope) VALUES (?, 'broad', 'claude', ?, 0, 0, 'Broad', ?, \
         'agent')",
    )
    .bind(BROAD_AGENT_ID)
    .bind(HOST_ID)
    .bind(USER_ID)
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
         created_by_user_id) VALUES (?, 'outsider', 'claude', ?, 0, 0, 'Outsider', ?)",
    )
    .bind(OUTSIDER_AGENT_ID)
    .bind(HOST_ID)
    .bind(USER_ID)
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspaces (id, name, name_key, created_at, updated_at, owner_user_id) VALUES \
         (?, 'Space', 'space', 0, 0, ?)",
    )
    .bind(WORKSPACE_ID)
    .bind(USER_ID)
    .execute(&mut *db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES (?, ?, 0)")
        .bind(WORKSPACE_ID)
        .bind(USER_ID)
        .execute(&mut *db)
        .await
        .unwrap();

    for (id, title_key) in [(CHANNEL_ID, "general"), (OTHER_CHANNEL_ID, "other")] {
        sqlx::query(
            "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
             title_key, status, created_at, updated_at) VALUES (?, ?, 'workspace', 'open', \
             'Channel', ?, 'open', 0, 0)",
        )
        .bind(id)
        .bind(WORKSPACE_ID)
        .bind(title_key)
        .execute(&mut *db)
        .await
        .unwrap();
        for agent in [AGENT_ID, BROAD_AGENT_ID] {
            sqlx::query(
                "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES (?, \
                 ?, 0)",
            )
            .bind(id)
            .bind(agent)
            .execute(&mut *db)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES (?, ?, 0)",
        )
        .bind(id)
        .bind(USER_ID)
        .execute(&mut *db)
        .await
        .unwrap();
    }

    // A direct pair carries its participants on the row: no roster rows exist for
    // it by construction.
    sqlx::query(
        "INSERT INTO conversations (id, placement_kind, roster_policy, title, status, \
         endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, updated_at) \
         VALUES (?, 'direct_pair', 'fixed', 'DM', 'open', 'agent', ?, 'human', ?, 0, 0)",
    )
    .bind(PAIR_ID)
    .bind(AGENT_ID)
    .bind(USER_ID)
    .execute(&mut *db)
    .await
    .unwrap();

    for (id, channel, filename, size, content_type, key) in [
        ("att-here", CHANNEL_ID, "note.txt", 12, "text/plain", "k1"),
        (
            "att-elsewhere",
            OTHER_CHANNEL_ID,
            "other.png",
            34,
            "image/png",
            "k2",
        ),
    ] {
        sqlx::query(
            "INSERT INTO attachments (id, channel_id, filename, size, content_type, storage, \
             storage_key, created_by_kind, created_by_id, created_at) VALUES (?, ?, ?, ?, ?, \
             'local', ?, 'user', ?, 0)",
        )
        .bind(id)
        .bind(channel)
        .bind(filename)
        .bind(size)
        .bind(content_type)
        .bind(key)
        .bind(USER_ID)
        .execute(&mut *db)
        .await
        .unwrap();
    }
}

fn agent_actor() -> SendActor {
    SendActor::Agent(AgentId::parse(AGENT_ID).unwrap())
}

fn user_actor() -> SendActor {
    SendActor::User(UserId::parse(USER_ID).unwrap())
}

/// A stand-in position rule: every `<…>` run, wherever it sits. The product rule is
/// Markdown structure — an autolink in prose, never quoted syntax or someone else's
/// quoted words — and is not implemented yet; what these tests exercise is the
/// identity half, which is read from the body's own bytes.
struct EveryBracketedRun;

impl ReferencePositions for EveryBracketedRun {
    fn autolink_spans(&self, content: &MessageContent) -> Vec<std::ops::Range<usize>> {
        let body = content.as_str();
        let mut spans = Vec::new();
        let mut cursor = 0;
        while let Some(open) = body[cursor..].find('<') {
            let open = cursor + open;
            let Some(close) = body[open..].find('>') else {
                break;
            };
            let close = open + close;
            spans.push(open..close + 1);
            cursor = close + 1;
        }
        spans
    }
}

fn draft(actor: SendActor, conversation: &str, content: &str) -> SendMessageDraft {
    SendMessageDraft {
        source_conversation: ConversationId::parse(conversation).unwrap(),
        target_conversation: ConversationId::parse(conversation).unwrap(),
        actor,
        content: MessageContent::parse(content).unwrap(),
        destination: SendDestination::MainLine,
        attachment_refs: Vec::new(),
        freshness_bypass: false,
    }
}

/// An authority that grants a direct line to every agent the body addressed. Tests
/// that are about the grant itself say so; the rest should not have to model it.
struct GrantsEveryAddressedAgent;

impl DirectLineAuthority for GrantsEveryAddressedAgent {
    async fn direct_lines(
        &self,
        _author: &UserId,
        addressed: &AddressedPrincipals,
    ) -> Result<Vec<AgentId>, PersistenceFailure> {
        Ok(addressed
            .iter()
            .filter(|reference| reference.kind() == PrincipalKind::Agent)
            .filter_map(|reference| AgentId::parse(reference.handle().as_str()).ok())
            .collect())
    }
}

/// An authority that grants the lines it was told to grant, mapping each handle to
/// the id it really names. A test about the copy path has to model this correctly:
/// the grant is about an id, and the handle is resolved again inside the unit.
struct Grants(Vec<(&'static str, &'static str)>);

impl DirectLineAuthority for Grants {
    async fn direct_lines(
        &self,
        _author: &UserId,
        addressed: &AddressedPrincipals,
    ) -> Result<Vec<AgentId>, PersistenceFailure> {
        Ok(addressed
            .iter()
            .filter(|reference| reference.kind() == PrincipalKind::Agent)
            .filter_map(|reference| {
                self.0
                    .iter()
                    .find(|(handle, _)| *handle == reference.handle().as_str())
                    .map(|(_, id)| AgentId::parse(*id).unwrap())
            })
            .collect())
    }
}

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the driver-free authority unexpectedly suspended"),
    }
}

fn prepared_with(draft: SendMessageDraft, authority: &impl DirectLineAuthority) -> SendMessage {
    ready(SendMessage::prepare(draft, &EveryBracketedRun, authority)).unwrap()
}

fn mentioning_outsider(conversation: &str) -> SendMessageDraft {
    draft(
        user_actor(),
        conversation,
        "<silverhand://agent/outsider> please look",
    )
}

fn grants_outsider() -> Grants {
    Grants(vec![("outsider", OUTSIDER_AGENT_ID)])
}

fn prepared(draft: SendMessageDraft) -> SendMessage {
    ready(SendMessage::prepare(
        draft,
        &EveryBracketedRun,
        &GrantsEveryAddressedAgent,
    ))
    .unwrap()
}

fn send(actor: SendActor, conversation: &str, content: &str) -> SendMessage {
    prepared(draft(actor, conversation, content))
}

/// A main-line send that skips the freshness guard, for when the sender legitimately
/// still holds an undelivered `turn` and the test is about a later fact — ordinals,
/// heads — rather than the guard itself.
fn send_bypassing_freshness(actor: SendActor, conversation: &str, content: &str) -> SendMessage {
    prepared(SendMessageDraft {
        freshness_bypass: true,
        ..draft(actor, conversation, content)
    })
}

/// The body a reply carries when it addresses the second agent by name.
fn addressing_broad(body: &str) -> String {
    format!("<silverhand://agent/broad> {body}")
}

/// A send whose target is not the conversation the sender is bound to.
fn send_across(actor: SendActor, source: &str, target: &str, content: &str) -> SendMessage {
    prepared(SendMessageDraft {
        source_conversation: ConversationId::parse(source).unwrap(),
        ..draft(actor, target, content)
    })
}

fn reply_to(actor: SendActor, conversation: &str, content: &str, target: &str) -> SendMessage {
    prepared(SendMessageDraft {
        destination: SendDestination::Reply(MessageId::parse(target).unwrap()),
        ..draft(actor, conversation, content)
    })
}

/// A reply that skips the freshness guard, for when the sender legitimately still
/// holds an undelivered `turn` from an earlier reply in the same cluster and the
/// test is about a later fact rather than the guard.
fn reply_bypassing_freshness(
    actor: SendActor,
    conversation: &str,
    content: &str,
    target: &str,
) -> SendMessage {
    prepared(SendMessageDraft {
        destination: SendDestination::Reply(MessageId::parse(target).unwrap()),
        freshness_bypass: true,
        ..draft(actor, conversation, content)
    })
}

fn into_thread(actor: SendActor, conversation: &str, content: &str, root: &str) -> SendMessage {
    prepared(SendMessageDraft {
        destination: SendDestination::Thread(ThreadRootId::parse(root).unwrap()),
        ..draft(actor, conversation, content)
    })
}

fn root_cause(error: &(dyn Error + 'static)) -> String {
    let mut current = error;
    while let Some(next) = current.source() {
        current = next;
    }
    current.to_string()
}

/// The main line writes the reserved ordinal, the message, the stream head, and —
/// in the same transaction — the durable delivery witness each recipient is owed: a
/// `turn` row for every participant except the sender. The durable *event* is still
/// a post-commit consequence, so `conversation_events` stays empty.
#[tokio::test]
async fn a_main_line_send_writes_the_ordinal_the_message_the_head_and_the_recipients_turn() {
    let mut fixture = Fixture::new().await;

    let fact = fixture
        .store()
        .await
        .send_message(send(agent_actor(), CHANNEL_ID, "first"))
        .await
        .unwrap()
        .committed()
        .clone();

    assert_eq!(fact.message().seq, MessageSeq::try_from(1).unwrap());
    assert_eq!(fact.thread(), None);
    assert!(!fact.freshness_bypassed());
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 1);
    assert_eq!(fixture.stream_head(CHANNEL_ID, None).await, Some(1));
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 1);
    // The one other participant is owed this message as a turn; the row rides this
    // send's transaction, not a later aftermath pass.
    assert_eq!(
        fixture.queued_for(BROAD_AGENT_ID).await,
        vec![(fact.message().id.as_str().to_owned(), "turn".to_owned())]
    );
    // The sender is never queued its own message.
    assert!(fixture.queued_for(AGENT_ID).await.is_empty());
    assert_eq!(
        fixture
            .count("SELECT count(*) FROM conversation_inboxes")
            .await,
        1
    );
    assert_eq!(
        fixture
            .count("SELECT count(*) FROM conversation_events")
            .await,
        0
    );
}

/// P0 — the current message's delivery witness rides the send's own transaction,
/// and it is written AFTER the send's other durable side effects. A mention send
/// writes real work before the witness: the DM copy message, its direct-pair
/// stream head, the outsider's copy inbox row, and the copy notification all land
/// earlier in this same transaction (that they appear at all on success is the
/// control in `every_effect_failing_leaves_the_committed_facts_in_place`). This
/// gate rejects ONLY the current channel's `turn` — the witness this fix added —
/// so those earlier effects are already written when it fails, and the whole unit
/// must roll back with them: no message, no ordinal, no head, no copy, no
/// direct-pair, no inbox on either side, no notification, no witness. If the
/// witness insert were moved after the commit, the message and its side effects
/// would survive the trigger and this gate would go red.
#[tokio::test]
async fn a_failed_current_message_witness_rolls_back_the_whole_send() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    // Reject the current channel's witness specifically. The outsider's copy inbox
    // row is a `turn` in the direct-pair line, not this channel, so it is written
    // first and unharmed; the trigger fires on the current-message witness for a
    // channel participant, which the fix writes only after the copy work is done.
    fixture
        .execute(
            "CREATE TRIGGER reject_current_turn BEFORE INSERT ON conversation_inboxes WHEN \
             NEW.delivery_kind = 'turn' AND NEW.conversation_id = 'conv-1' BEGIN \
             SELECT RAISE(ABORT, 'forced witness failure'); END",
        )
        .await;

    let error = store
        .send_message(prepared_with(
            mentioning_outsider(CHANNEL_ID),
            &grants_outsider(),
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Persistence(_)));
    // Every fact this send would have written is gone with the transaction — both
    // the current-message witness AND the copy work that preceded it.
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 0);
    assert_eq!(
        fixture
            .count("SELECT count(*) FROM messages WHERE cc_source_message_id IS NOT NULL")
            .await,
        0
    );
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 0);
    assert_eq!(fixture.stream_head(CHANNEL_ID, None).await, None);
    assert!(
        fixture.direct_pairs().await.is_empty(),
        "the copy's direct-pair line rolled back with the send"
    );
    assert_eq!(
        fixture
            .count("SELECT count(*) FROM conversation_inboxes")
            .await,
        0,
        "neither the copy inbox row nor the current witness survives"
    );
    assert!(fixture.queued_for(OUTSIDER_AGENT_ID).await.is_empty());
    assert_eq!(
        fixture
            .count("SELECT count(*) FROM conversation_events")
            .await,
        0
    );
    assert!(
        fixture.notification_rows().await.is_empty(),
        "the copy notification rolled back too"
    );
    assert_eq!(
        fixture.count("SELECT count(*) FROM thread_followers").await,
        0
    );
}

#[tokio::test]
async fn the_committed_row_reports_the_same_facts_the_caller_was_given() {
    let fixture = Fixture::new().await;

    let fact = fixture
        .store()
        .await
        .send_message(send(user_actor(), CHANNEL_ID, "spoken once"))
        .await
        .unwrap()
        .committed()
        .clone();

    let mut fixture = fixture;
    let row = sqlx::query(
        "SELECT id, from_kind, from_agent_id, from_user_id, content, metadata, seq, created_at, \
         thread_root_id, author_id, author_name, author_display_name, cc_source_message_id FROM \
         messages WHERE id = ?",
    )
    .bind(fact.message().id.as_str())
    .fetch_one(&mut fixture.db)
    .await
    .unwrap();

    assert_eq!(row.get::<String, _>("from_kind"), "user");
    assert_eq!(row.get::<Option<String>, _>("from_agent_id"), None);
    assert_eq!(
        row.get::<Option<String>, _>("from_user_id").as_deref(),
        Some(USER_ID)
    );
    assert_eq!(row.get::<String, _>("content"), "spoken once");
    assert_eq!(row.get::<Option<String>, _>("metadata"), None);
    assert_eq!(row.get::<i64, _>("seq"), fact.message().seq.get());
    assert_eq!(
        row.get::<i64, _>("created_at"),
        fact.message().created_at_ms
    );
    assert_eq!(row.get::<Option<String>, _>("thread_root_id"), None);
    assert_eq!(row.get::<Option<String>, _>("cc_source_message_id"), None);
    // The author is copied onto the row, so the message keeps naming its sender
    // after a rename or a deletion.
    assert_eq!(
        row.get::<Option<String>, _>("author_name").as_deref(),
        Some("human")
    );
    assert_eq!(
        row.get::<Option<String>, _>("author_display_name")
            .as_deref(),
        Some("Human")
    );
    // The identity the caller was handed is the identity that was stored.
    assert_eq!(row.get::<String, _>("id"), fact.message().id.as_str());
    assert_eq!(fact.message().id.as_str().len(), 16);
}

/// The returned author is the sender resolved live: an agent reports its current
/// name, display and runtime. Unlike the send-time snapshot columns, this arm would
/// track a later rename or new avatar, because the intact FK resolves the live row.
#[tokio::test]
async fn the_returned_author_of_an_agent_send_is_the_live_agent_with_its_runtime() {
    let fixture = Fixture::new().await;

    let fact = fixture
        .store()
        .await
        .send_message(send(agent_actor(), CHANNEL_ID, "hello"))
        .await
        .unwrap()
        .committed()
        .clone();

    let Some(MessageAuthor::Active(ActiveAuthor::Agent(agent))) = fact.message().author.as_ref()
    else {
        panic!("expected an active agent author");
    };
    assert_eq!(agent.id().as_str(), AGENT_ID);
    assert_eq!(agent.name(), "lio");
    assert_eq!(agent.display_name(), "Lio");
    assert_eq!(agent.runtime(), RuntimeKind::Claude);
    // The seed agent carries no avatar storage key, so there is no revision.
    assert_eq!(agent.avatar(), None);
}

/// A human send resolves to an active human with no runtime — a human has none, so
/// the arm does not carry one at all.
#[tokio::test]
async fn the_returned_author_of_a_human_send_is_the_live_human_without_a_runtime() {
    let fixture = Fixture::new().await;

    let fact = fixture
        .store()
        .await
        .send_message(send(user_actor(), CHANNEL_ID, "hi"))
        .await
        .unwrap()
        .committed()
        .clone();

    let Some(MessageAuthor::Active(ActiveAuthor::Human(human))) = fact.message().author.as_ref()
    else {
        panic!("expected an active human author");
    };
    assert_eq!(human.id().as_str(), USER_ID);
    assert_eq!(human.name(), "human");
    assert_eq!(human.display_name(), "Human");
    assert_eq!(human.avatar(), None);
}

/// A stored avatar key resolves to a revision the wire busts its cache on. The
/// revision is the row's live `updated_at`, so it tracks the current avatar rather
/// than any value fixed at send.
#[tokio::test]
async fn the_returned_author_carries_the_avatar_revision_of_a_live_actor() {
    let mut fixture = Fixture::new().await;
    fixture
        .execute(
            "UPDATE agents SET avatar_storage_key = 'k', avatar_content_type = 'image/png', \
             updated_at = 999 WHERE id = 'agent-1'",
        )
        .await;

    let fact = fixture
        .store()
        .await
        .send_message(send(agent_actor(), CHANNEL_ID, "hello"))
        .await
        .unwrap()
        .committed()
        .clone();

    let Some(MessageAuthor::Active(ActiveAuthor::Agent(agent))) = fact.message().author.as_ref()
    else {
        panic!("expected an active agent author");
    };
    assert_eq!(agent.avatar().map(|a| a.cache_buster()), Some(999));
}

#[tokio::test]
async fn ordinals_are_conversation_global_and_a_thread_keeps_its_own_head() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    let reply = store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "reply", &root_id))
        .await
        .unwrap()
        .committed()
        .clone();
    // The reply queued lio a `turn`, so its own next main-line send would trip the
    // freshness guard; this test is about ordinals, so it bypasses.
    let after = store
        .send_message(send_bypassing_freshness(
            agent_actor(),
            CHANNEL_ID,
            "main line again",
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    // The reply draws from the conversation's counter, not from a per-thread one.
    assert_eq!(reply.message().seq, MessageSeq::try_from(2).unwrap());
    assert_eq!(after.message().seq, MessageSeq::try_from(3).unwrap());
    assert_eq!(
        reply.message().thread_root_id.as_ref().unwrap().as_str(),
        root_id
    );
    assert_eq!(
        fixture.stream_head(CHANNEL_ID, Some(&root_id)).await,
        Some(2)
    );
    assert_eq!(fixture.stream_head(CHANNEL_ID, None).await, Some(3));
}

#[tokio::test]
async fn ordinals_of_two_conversations_do_not_share_a_counter() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;

    store
        .send_message(send(agent_actor(), CHANNEL_ID, "here"))
        .await
        .unwrap();
    let elsewhere = store
        .send_message(send(agent_actor(), OTHER_CHANNEL_ID, "there"))
        .await
        .unwrap()
        .committed()
        .clone();

    assert_eq!(elsewhere.message().seq, MessageSeq::try_from(1).unwrap());
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 1);
    assert_eq!(fixture.latest_seq(OTHER_CHANNEL_ID).await, 1);
}

#[tokio::test]
async fn the_main_line_head_is_updated_rather_than_duplicated() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;

    for _ in 0..3 {
        store
            .send_message(send(agent_actor(), CHANNEL_ID, "again"))
            .await
            .unwrap();
    }

    assert_eq!(
        fixture
            .count("SELECT count(*) FROM conversation_stream_heads")
            .await,
        1
    );
    assert_eq!(fixture.stream_head(CHANNEL_ID, None).await, Some(3));
}

/// The reply count is absolute and computed after the insert, so it already
/// includes the reply being reported; system copies are not participants' words
/// and stay out of it.
#[tokio::test]
async fn a_reply_reports_an_absolute_count_that_excludes_system_copies() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    let first = store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "one", &root_id))
        .await
        .unwrap()
        .committed()
        .clone();

    sqlx::query(
        "INSERT INTO messages (id, conversation_id, from_kind, content, seq, created_at, \
         thread_root_id) VALUES ('sys-copy-000000', ?, 'system', 'copied', 99, 0, ?)",
    )
    .bind(CHANNEL_ID)
    .bind(&root_id)
    .execute(&mut fixture.db)
    .await
    .unwrap();

    let second = store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "two", &root_id))
        .await
        .unwrap()
        .committed()
        .clone();

    assert_eq!(first.thread().unwrap().reply_count(), 1);
    let thread = second.thread().unwrap();
    assert_eq!(thread.reply_count(), 2);
    assert_eq!(thread.thread_root_id().as_str(), root_id);
}

/// A reply to a message of another conversation is refused as if the message did
/// not exist, so a reply cannot be used to probe another stream.
#[tokio::test]
async fn a_reply_target_outside_the_conversation_is_not_found() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let elsewhere = store
        .send_message(send(agent_actor(), OTHER_CHANNEL_ID, "far away"))
        .await
        .unwrap()
        .committed()
        .clone();

    let error = store
        .send_message(reply_to(
            agent_actor(),
            CHANNEL_ID,
            "reply",
            elsewhere.message().id.as_str(),
        ))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SendMessageError::ReplyTarget(ReplyTargetRefusal)
    ));
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 0);
}

/// A named cluster is checked here, not trusted from whoever resolved the address.
/// Unknown, foreign and "that message is itself a reply" are one refusal, and none
/// of them may reach the database as a constraint violation.
#[tokio::test]
async fn a_thread_root_that_names_no_cluster_of_this_conversation_is_refused() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let elsewhere = store
        .send_message(send(agent_actor(), OTHER_CHANNEL_ID, "far away"))
        .await
        .unwrap()
        .committed()
        .clone();
    let mid_cluster = store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            "a reply",
            root.message().id.as_str(),
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    let seq_before = fixture.latest_seq(CHANNEL_ID).await;

    for candidate in [
        "AAAAAAAAAAAAAAAA",
        elsewhere.message().id.as_str(),
        mid_cluster.message().id.as_str(),
    ] {
        let error = store
            .send_message(into_thread(agent_actor(), CHANNEL_ID, "into it", candidate))
            .await
            .unwrap_err();

        assert!(
            matches!(error, SendMessageError::ThreadRoot(ThreadRootRefusal)),
            "root {candidate} was not refused as a cluster name: {error}"
        );
    }
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, seq_before);
}

#[tokio::test]
async fn a_send_into_a_named_cluster_joins_that_cluster() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    let fact = store
        .send_message(into_thread(
            user_actor(),
            CHANNEL_ID,
            "in the cluster",
            &root_id,
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    assert_eq!(
        fact.message().thread_root_id.as_ref().unwrap().as_str(),
        root_id
    );
    assert_eq!(fact.thread().unwrap().reply_count(), 1);
    assert_eq!(
        fixture.stream_head(CHANNEL_ID, Some(&root_id)).await,
        Some(2)
    );
}

#[tokio::test]
async fn a_closed_conversation_answers_with_its_lifecycle() {
    let mut fixture = Fixture::new().await;
    fixture
        .execute("UPDATE conversations SET closed_at = 1 WHERE id = 'conv-1'")
        .await;

    let error = fixture
        .store()
        .await
        .send_message(send(agent_actor(), CHANNEL_ID, "too late"))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SendMessageError::Conversation(ConversationRefusal::Closed)
    ));
}

#[tokio::test]
async fn an_archived_conversation_answers_with_its_lifecycle() {
    let mut fixture = Fixture::new().await;
    fixture
        .execute("UPDATE conversations SET archived_at = 1 WHERE id = 'conv-1'")
        .await;

    let error = fixture
        .store()
        .await
        .send_message(send(agent_actor(), CHANNEL_ID, "too late"))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SendMessageError::Conversation(ConversationRefusal::Archived)
    ));
}

/// A refused send leaves the conversation exactly as it was: the ordinal it would
/// have consumed is still unissued.
#[tokio::test]
async fn a_refused_send_consumes_no_ordinal() {
    let mut fixture = Fixture::new().await;
    fixture
        .execute(
            "DELETE FROM conversation_agents WHERE conversation_id = 'conv-1' AND agent_id = \
             'agent-1'",
        )
        .await;

    let error = fixture
        .store()
        .await
        .send_message(send(agent_actor(), CHANNEL_ID, "outsider"))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SendMessageError::Actor(ActorRefusal::AgentNotParticipant)
    ));
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 0);
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 0);
    assert_eq!(
        fixture
            .count("SELECT count(*) FROM conversation_stream_heads")
            .await,
        0
    );
}

/// A direct pair has no roster rows: its participants are the endpoints on the
/// row, and a roster-shaped question would refuse both of them.
#[tokio::test]
async fn a_direct_pair_endpoint_participates_without_a_roster_row() {
    let fixture = Fixture::with_scope("agent").await;
    let store = fixture.store().await;

    let by_agent = store
        .send_message(send(agent_actor(), PAIR_ID, "from the agent"))
        .await
        .unwrap()
        .committed()
        .clone();
    let by_human = store
        .send_message(send(user_actor(), PAIR_ID, "from the human"))
        .await
        .unwrap()
        .committed()
        .clone();

    assert_eq!(by_agent.message().seq, MessageSeq::try_from(1).unwrap());
    assert_eq!(by_human.message().seq, MessageSeq::try_from(2).unwrap());
}

#[tokio::test]
async fn a_pair_whose_endpoint_departed_refuses_its_survivor() {
    let mut fixture = Fixture::new().await;
    fixture
        .execute("UPDATE conversations SET endpoint_b_deleted_at = 1 WHERE id = 'pair-1'")
        .await;

    let error = fixture
        .store()
        .await
        .send_message(send(agent_actor(), PAIR_ID, "anyone there"))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SendMessageError::Conversation(ConversationRefusal::EndpointGone)
    ));
}

/// A person's channel seat is two facts. A seat that outlived the workspace
/// membership admits nobody, because the membership is what the seat was granted
/// under.
#[tokio::test]
async fn a_human_needs_the_seat_and_the_workspace_membership() {
    let mut fixture = Fixture::new().await;
    fixture
        .execute("DELETE FROM workspace_users WHERE workspace_id = 'ws-1' AND user_id = 'user-1'")
        .await;

    let error = fixture
        .store()
        .await
        .send_message(send(user_actor(), CHANNEL_ID, "still seated"))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SendMessageError::Actor(ActorRefusal::UserNotParticipant)
    ));
}

#[tokio::test]
async fn an_attachment_of_another_channel_is_refused_and_nothing_is_written() {
    let mut fixture = Fixture::new().await;
    let command = prepared(SendMessageDraft {
        attachment_refs: vec![AttachmentRef {
            id: AttachmentId::parse("att-elsewhere").unwrap(),
            label: None,
        }],
        ..draft(user_actor(), CHANNEL_ID, "look at this")
    });

    let error = fixture
        .store()
        .await
        .send_message(command)
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Attachment(_)));
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 0);
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 0);
}

#[tokio::test]
async fn an_accepted_attachment_is_stored_as_a_snapshot_in_the_message_metadata() {
    let mut fixture = Fixture::new().await;
    let command = prepared(SendMessageDraft {
        attachment_refs: vec![AttachmentRef {
            id: AttachmentId::parse("att-here").unwrap(),
            label: Some("File #1".to_owned()),
        }],
        ..draft(user_actor(), CHANNEL_ID, "with a file")
    });

    let fact = fixture
        .store()
        .await
        .send_message(command)
        .await
        .unwrap()
        .committed()
        .clone();

    let metadata: String = sqlx::query_scalar("SELECT metadata FROM messages WHERE id = ?")
        .bind(fact.message().id.as_str())
        .fetch_one(&mut fixture.db)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&metadata).unwrap(),
        serde_json::json!({
            "attachments": [{
                "id": "att-here",
                "filename": "note.txt",
                "size": 12,
                "content_type": "text/plain",
                "label": "File #1",
            }]
        })
    );
    assert_eq!(fact.message().attachments.len(), 1);
}

fn task_events_by(
    actor: &str,
    assignee: &str,
) -> Vec<silverhand_backend_core::conversation_messaging::messaging::task::TaskEventMessage> {
    use silverhand_backend_core::conversation_messaging::messaging::task::*;
    let facts = TaskEventFacts {
        task_id: TaskId::parse("task-own-event").unwrap(),
        task_number: TaskNumber::new(1).unwrap(),
        title: TaskTitle::parse("Work").unwrap(),
        status: TaskStatus::InProgress,
        actor: TaskSystemActor::Agent {
            id: AgentId::parse(actor).unwrap(),
            names: AgentEventNames::new("actor", "Actor"),
        },
        assignee: Some(TaskEventAssignee::Agent {
            id: AgentId::parse(assignee).unwrap(),
            names: AgentEventNames::new("assignee", "Assignee"),
        }),
    };
    vec![
        compose_create_event(&facts, CreatePlacement::Filed),
        compose_edit_event(&facts),
        compose_status_event(&facts),
        compose_claim_event(&facts),
        compose_unclaim_event(&facts, "Assignee"),
    ]
}

async fn enqueue_task_event(fixture: &mut Fixture, conversation: &str, id: &str, metadata: &str) {
    let seq = fixture.latest_seq(conversation).await + 1;
    sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,content,metadata,seq,created_at) VALUES(?,?,'system','task event',?,?,1)")
        .bind(id).bind(conversation).bind(metadata).bind(seq).execute(&mut fixture.db).await.unwrap();
    sqlx::query("UPDATE conversations SET latest_message_seq=? WHERE id=?")
        .bind(seq)
        .bind(conversation)
        .execute(&mut fixture.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversation_inboxes(conversation_id,agent_id,message_id,created_at,delivery_kind) VALUES(?,?,?,1,'turn')")
        .bind(conversation).bind(AGENT_ID).bind(id).execute(&mut fixture.db).await.unwrap();
}

#[tokio::test]
async fn self_authored_task_events_do_not_block_either_scope_or_settle_the_inbox() {
    for scope in ["conversation", "agent"] {
        for event in task_events_by(AGENT_ID, BROAD_AGENT_ID) {
            let mut fixture = Fixture::with_scope(scope).await;
            let event_conversation = if scope == "agent" {
                OTHER_CHANNEL_ID
            } else {
                CHANNEL_ID
            };
            enqueue_task_event(
                &mut fixture,
                event_conversation,
                "self-event",
                &event.metadata,
            )
            .await;
            fixture.execute("UPDATE conversation_inboxes SET in_flight_owner='runtime',in_flight_at=1 WHERE agent_id='agent-1'").await;
            let before = fixture.witness_rows(AGENT_ID).await;
            let fact = fixture
                .store()
                .await
                .send_message(send(agent_actor(), CHANNEL_ID, "continue"))
                .await
                .unwrap()
                .committed()
                .clone();
            assert!(!fact.freshness_bypassed());
            assert_eq!(
                fixture.witness_rows(AGENT_ID).await,
                before,
                "ignoring self input cannot acknowledge or release a lease"
            );
        }
    }
}

#[tokio::test]
async fn mixed_self_and_foreign_task_events_keep_the_foreign_input_blocking_atomically() {
    for scope in ["conversation", "agent"] {
        for foreign in task_events_by(BROAD_AGENT_ID, AGENT_ID) {
            let mut fixture = Fixture::with_scope(scope).await;
            let event_conversation = if scope == "agent" {
                OTHER_CHANNEL_ID
            } else {
                CHANNEL_ID
            };
            let own = task_events_by(AGENT_ID, BROAD_AGENT_ID);
            enqueue_task_event(
                &mut fixture,
                event_conversation,
                "self-before",
                &own[0].metadata,
            )
            .await;
            enqueue_task_event(
                &mut fixture,
                event_conversation,
                "foreign",
                &foreign.metadata,
            )
            .await;
            enqueue_task_event(
                &mut fixture,
                event_conversation,
                "self-after",
                &own[1].metadata,
            )
            .await;
            let store = fixture.store().await;
            let before = fixture.witness_rows(AGENT_ID).await;
            let seq = fixture.latest_seq(CHANNEL_ID).await;
            assert!(matches!(
                store
                    .send_message(send(agent_actor(), CHANNEL_ID, "too soon"))
                    .await,
                Err(SendMessageError::Stale(_))
            ));
            assert_eq!(
                fixture.latest_seq(CHANNEL_ID).await,
                seq,
                "stale rejects before ordinal allocation"
            );
            assert_eq!(fixture.witness_rows(AGENT_ID).await, before);
            fixture
                .execute(
                    "UPDATE conversation_inboxes SET delivered_at=1 WHERE message_id='foreign'",
                )
                .await;
            store
                .send_message(send(agent_actor(), CHANNEL_ID, "now fresh"))
                .await
                .unwrap();
            assert_eq!(fixture.count("SELECT COUNT(*) FROM conversation_inboxes WHERE agent_id='agent-1' AND delivered_at IS NULL").await, 2);
        }
    }
}

#[tokio::test]
async fn actor_metadata_is_fail_closed_and_cannot_be_spoofed_by_a_normal_message() {
    for metadata in [
        None,
        Some("not-json"),
        Some(r#"{"subtype":"task_status","task_assignee":{"type":"agent","id":"agent-1"}}"#),
        Some(r#"{"subtype":"task_status","actor":{"kind":"user","id":"agent-1"}}"#),
        Some(r#"{"subtype":"unknown","actor":{"kind":"agent","id":"agent-1"}}"#),
        Some(r#"{"subtype":"task_edit","actor":{"kind":"agent","id":1}}"#),
    ] {
        let mut fixture = Fixture::new().await;
        enqueue_task_event(
            &mut fixture,
            CHANNEL_ID,
            "unknown",
            metadata.unwrap_or("null"),
        )
        .await;
        assert!(matches!(
            fixture
                .store()
                .await
                .send_message(send(agent_actor(), CHANNEL_ID, "must read"))
                .await,
            Err(SendMessageError::Stale(_))
        ));
    }
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let message = store
        .send_message(send(user_actor(), CHANNEL_ID, "human input"))
        .await
        .unwrap()
        .committed()
        .message()
        .id
        .clone();
    sqlx::query("UPDATE messages SET metadata=? WHERE id=?")
        .bind(&task_events_by(AGENT_ID, AGENT_ID)[0].metadata)
        .bind(message.as_str())
        .execute(&mut fixture.db)
        .await
        .unwrap();
    assert!(matches!(
        store
            .send_message(send(agent_actor(), CHANNEL_ID, "cannot spoof"))
            .await,
        Err(SendMessageError::Stale(_))
    ));
}

#[tokio::test]
async fn an_agent_holding_undelivered_input_is_refused_as_stale() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    // The user's send now writes the agent's pending `turn` row in the same
    // transaction, so the undelivered input the guard trips on is the real witness
    // this send produced — not a hand-seeded stand-in.
    let read_me = store
        .send_message(send(user_actor(), CHANNEL_ID, "read me first"))
        .await
        .unwrap()
        .committed()
        .clone();
    // The exact witness: one pending `turn` for this conversation and message, with
    // every settlement column still empty — the row the stale guard reads next.
    assert_eq!(
        fixture.witness_rows(AGENT_ID).await,
        vec![InboxWitness {
            conversation_id: CHANNEL_ID.to_owned(),
            message_id: read_me.message().id.as_str().to_owned(),
            delivery_kind: "turn".to_owned(),
            delivered_at: None,
            failed_at: None,
            in_flight_owner: None,
        }]
    );

    let error = store
        .send_message(send(agent_actor(), CHANNEL_ID, "talking over it"))
        .await
        .unwrap_err();

    let SendMessageError::Stale(refusal) = &error else {
        panic!("expected a stale refusal, got {error}");
    };
    assert_eq!(refusal.target_conversation_id.as_str(), CHANNEL_ID);
    assert_eq!(refusal.stale_event_id.as_str().len(), 36);
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 1);
}

/// The guard counts rows a runtime cannot yet drain, so an in-flight lease still
/// reads as undelivered.
#[tokio::test]
async fn an_in_flight_delivery_still_counts_as_undelivered() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    // The send itself queues the agent's pending `turn` row; leasing it in flight
    // must still read as undelivered.
    store
        .send_message(send(user_actor(), CHANNEL_ID, "leased"))
        .await
        .unwrap();
    fixture
        .execute(
            "UPDATE conversation_inboxes SET in_flight_owner = 'runtime-1', in_flight_at = 1 WHERE \
             agent_id = 'agent-1'",
        )
        .await;

    let error = store
        .send_message(send(agent_actor(), CHANNEL_ID, "talking over it"))
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Stale(_)));
}

#[tokio::test]
async fn a_delivered_row_leaves_the_runtime_fresh() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    // The send queues the agent's pending `turn`; draining it (delivered_at set)
    // must leave the runtime free to answer.
    store
        .send_message(send(user_actor(), CHANNEL_ID, "consumed"))
        .await
        .unwrap();
    fixture
        .execute("UPDATE conversation_inboxes SET delivered_at = 1 WHERE agent_id = 'agent-1'")
        .await;

    let after_drain = store
        .send_message(send(agent_actor(), CHANNEL_ID, "my turn"))
        .await
        .unwrap()
        .committed()
        .clone();

    assert_eq!(after_drain.message().seq, MessageSeq::try_from(2).unwrap());
    assert!(!after_drain.freshness_bypassed());
}

#[tokio::test]
async fn sending_anyway_lands_and_records_that_the_guard_was_skipped() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    // The send queues the agent's pending `turn`; the bypass lands over it.
    store
        .send_message(send(user_actor(), CHANNEL_ID, "unread"))
        .await
        .unwrap();

    let command = prepared(SendMessageDraft {
        freshness_bypass: true,
        ..draft(agent_actor(), CHANNEL_ID, "anyway")
    });
    let fact = store
        .send_message(command)
        .await
        .unwrap()
        .committed()
        .clone();

    assert!(fact.freshness_bypassed());
    assert_eq!(fact.message().seq, MessageSeq::try_from(2).unwrap());
}

/// A person has no runtime, so undelivered agent input cannot make a human send
/// stale.
#[tokio::test]
async fn a_human_send_is_never_stale() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    // The first send leaves the agent an undelivered `turn`; a person has no
    // runtime, so that cannot gag the human's next send.
    store
        .send_message(send(user_actor(), CHANNEL_ID, "unread by the agent"))
        .await
        .unwrap();

    let fact = store
        .send_message(send(user_actor(), CHANNEL_ID, "people keep talking"))
        .await
        .unwrap()
        .committed()
        .clone();

    assert_eq!(fact.message().seq, MessageSeq::try_from(2).unwrap());
}

/// The conversation arm asks about the runtime's own binding, so undelivered input
/// in one conversation does not gag a runtime bound to another.
#[tokio::test]
async fn the_conversation_arm_keys_on_the_runtimes_binding() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    // The send queues the agent an undelivered `turn` in the other channel; a
    // conversation-bound runtime does not see it.
    store
        .send_message(send(user_actor(), OTHER_CHANNEL_ID, "elsewhere"))
        .await
        .unwrap();

    let fact = store
        .send_message(send(agent_actor(), CHANNEL_ID, "unrelated stream"))
        .await
        .unwrap()
        .committed()
        .clone();

    assert_eq!(fact.message().seq, MessageSeq::try_from(1).unwrap());
}

/// An agent-scoped runtime sees its whole field of view, so undelivered input
/// anywhere makes it stale.
#[tokio::test]
async fn the_agent_arm_spans_every_conversation() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    // The send queues broad an undelivered `turn` in the other channel; an
    // agent-scoped runtime sees it everywhere.
    store
        .send_message(send(user_actor(), OTHER_CHANNEL_ID, "elsewhere"))
        .await
        .unwrap();

    let broad = SendActor::Agent(AgentId::parse(BROAD_AGENT_ID).unwrap());
    let error = store
        .send_message(send(broad, CHANNEL_ID, "unrelated stream"))
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Stale(_)));
}

#[tokio::test]
async fn content_is_stored_byte_exact_at_the_utf8_bound() {
    let mut fixture = Fixture::new().await;
    let content = "汉".repeat(MAX_MESSAGE_CONTENT_BYTES / 3);

    let fact = fixture
        .store()
        .await
        .send_message(send(user_actor(), CHANNEL_ID, &content))
        .await
        .unwrap()
        .committed()
        .clone();

    let stored: String = sqlx::query_scalar("SELECT content FROM messages WHERE id = ?")
        .bind(fact.message().id.as_str())
        .fetch_one(&mut fixture.db)
        .await
        .unwrap();
    assert_eq!(stored, content);
    assert_eq!(stored.len(), 65_535);
}

/// A stored ordinal the domain rejects is a broken database, not a refusal: it is
/// reported as infrastructure, with the offending value in the cause, and nothing
/// is written.
#[tokio::test]
async fn a_negative_stored_ordinal_fails_as_a_data_invariant() {
    let mut fixture = Fixture::new().await;
    fixture
        .execute("UPDATE conversations SET latest_message_seq = -5 WHERE id = 'conv-1'")
        .await;

    let error = fixture
        .store()
        .await
        .send_message(send(agent_actor(), CHANNEL_ID, "impossible ordinal"))
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Persistence(_)));
    assert!(
        root_cause(&error).contains("-4"),
        "the offending ordinal was not reported: {}",
        root_cause(&error)
    );
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 0);
}

/// A driver failure reaches the caller as an infrastructure failure with its cause
/// intact, and rolls the unit back.
#[tokio::test]
async fn a_driver_failure_rolls_back_and_keeps_its_cause() {
    let mut fixture = Fixture::new().await;
    fixture
        .execute(
            "CREATE TRIGGER reject_test_message BEFORE INSERT ON messages BEGIN SELECT \
             RAISE(ABORT, 'forced insert failure'); END",
        )
        .await;

    let error = fixture
        .store()
        .await
        .send_message(send(agent_actor(), CHANNEL_ID, "never lands"))
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Persistence(_)));
    assert!(
        root_cause(&error).contains("forced insert failure"),
        "driver cause was lost: {}",
        root_cause(&error)
    );
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 0);
}

#[tokio::test]
async fn a_send_into_a_conversation_that_does_not_exist_is_not_found() {
    let fixture = Fixture::new().await;

    let error = fixture
        .store()
        .await
        .send_message(send(agent_actor(), "conv-absent", "into the void"))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SendMessageError::Conversation(ConversationRefusal::NotFound)
    ));
}

#[derive(Debug, thiserror::Error)]
#[error("the host capability was withdrawn for this test")]
struct Withdrawn;

/// A host that cannot answer fails the send before a transaction exists, so the
/// database is untouched and the reason survives to the caller.
struct BrokenClock;

impl SendClock for BrokenClock {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        Err(EnvironmentUnavailable::from_source(Withdrawn))
    }
}

impl SendEntropy for BrokenClock {
    fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        HostEnvironment.fill(buffer)
    }
}

struct BrokenEntropy;

impl SendClock for BrokenEntropy {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        Ok(1)
    }
}

impl SendEntropy for BrokenEntropy {
    fn fill(&self, _buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        Err(EnvironmentUnavailable::from_source(Withdrawn))
    }
}

#[tokio::test]
async fn a_clock_that_cannot_answer_writes_nothing_and_keeps_its_reason() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store_with(BrokenClock).await;

    let error = store
        .send_message(send(agent_actor(), CHANNEL_ID, "when was this"))
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Environment(_)));
    assert_eq!(
        root_cause(&error),
        "the host capability was withdrawn for this test"
    );
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 0);
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 0);
}

#[tokio::test]
async fn an_entropy_source_that_cannot_answer_writes_nothing_and_keeps_its_reason() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store_with(BrokenEntropy).await;

    let error = store
        .send_message(send(agent_actor(), CHANNEL_ID, "who am i"))
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Environment(_)));
    assert_eq!(
        root_cause(&error),
        "the host capability was withdrawn for this test"
    );
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 0);
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 0);
}

/// Entropy that answers a fixed number of times and then withdraws. The stale
/// refusal mints its own identity INSIDE the transaction, so this is the second
/// call, and its failure has to leave the reserved writer with nothing written.
struct EntropyExhaustedAfter {
    allowance: usize,
    calls: AtomicUsize,
}

impl SendClock for EntropyExhaustedAfter {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        Ok(1)
    }
}

impl SendEntropy for EntropyExhaustedAfter {
    fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        if self.calls.fetch_add(1, Ordering::SeqCst) >= self.allowance {
            return Err(EnvironmentUnavailable::from_source(Withdrawn));
        }
        HostEnvironment.fill(buffer)
    }
}

#[tokio::test]
async fn entropy_withdrawn_inside_the_unit_rolls_back_with_nothing_written() {
    let mut fixture = Fixture::new().await;
    let seeded = fixture.store().await;
    // The user's send queues the agent a pending `turn`, so the agent's own send is
    // stale and must mint a fresh stale-event identity.
    seeded
        .send_message(send(user_actor(), CHANNEL_ID, "read me first"))
        .await
        .unwrap();
    let store = fixture
        .store_with(EntropyExhaustedAfter {
            allowance: 1,
            calls: AtomicUsize::new(0),
        })
        .await;

    // The send is stale, so the unit reaches for a second identity — and this host
    // has none left.
    let error = store
        .send_message(send(agent_actor(), CHANNEL_ID, "talking over it"))
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Environment(_)));
    assert_eq!(
        root_cause(&error),
        "the host capability was withdrawn for this test"
    );
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 1);
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 1);
}

/// A reply's own author and the author of the cluster's root both follow it, and
/// the reason each of them follows is recorded.
#[tokio::test]
async fn a_reply_records_its_author_and_the_root_author_as_followers() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "answering", &root_id))
        .await
        .unwrap();

    assert_eq!(
        fixture.followers(&root_id).await,
        vec![
            (
                "agent".to_owned(),
                AGENT_ID.to_owned(),
                "target_author".to_owned()
            ),
            ("human".to_owned(), USER_ID.to_owned(), "reply".to_owned()),
        ]
    );
    // A person reads a conversation; only an agent is delivered one, so becoming a
    // human follower queues nothing.
    assert!(fixture.queued_for(USER_ID).await.is_empty());
}

/// A newly followed agent's catch-up covers the cluster it had not seen — but only
/// the messages it had not seen. A channel participant already holds the root as a
/// live `turn`, written earlier than this catch-up would run. The unique key is
/// `(conversation, agent, message)` and does NOT include `delivery_kind`, so the
/// catch-up's `observe(root)` INSERT hits `ON CONFLICT DO NOTHING`: it is dropped
/// because a row for that message already exists — first writer wins, not because
/// `turn` outranks `observe`. (Had the `observe` been written first, a later `turn`
/// for the same message would be dropped just the same.) The reply it genuinely
/// missed arrives as `observe`; the reply that named it is delivered in the same
/// transaction as a `turn`. Catch-up `observe` and current-turn coexist without
/// duplicating a row.
#[tokio::test]
async fn a_mentioned_participant_is_followed_and_caught_up_on_the_cluster() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();
    let earlier = store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            "earlier reply",
            &root_id,
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    // The earlier reply queued lio a `turn`, so its own mentioning reply would trip
    // the freshness guard; the guard is not what this test is about, so it bypasses.
    let mentioning = store
        .send_message(reply_bypassing_freshness(
            agent_actor(),
            CHANNEL_ID,
            &addressing_broad("look"),
            &root_id,
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    assert!(fixture.followers(&root_id).await.contains(&(
        "agent".to_owned(),
        BROAD_AGENT_ID.to_owned(),
        "mention".to_owned()
    )));
    assert_eq!(
        fixture.queued_for(BROAD_AGENT_ID).await,
        vec![
            // Already held as a live participant turn — catch-up cannot downgrade it.
            (root_id.clone(), "turn".to_owned()),
            // Genuinely missed while not following — queued as history.
            (
                earlier.message().id.as_str().to_owned(),
                "observe".to_owned()
            ),
            // The reply that named it, delivered in the same transaction as a turn.
            (
                mentioning.message().id.as_str().to_owned(),
                "turn".to_owned()
            ),
        ],
        "catch-up must add only the unseen reply and never duplicate the root row"
    );
}

/// Following is idempotent, so the `observe` history a catch-up queues is written
/// once however many times a follower is named again — even as each naming reply
/// still delivers its own `turn`.
#[tokio::test]
async fn following_a_cluster_twice_queues_its_history_once() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    // A reply broad does not receive: it is not yet a follower, so this becomes the
    // one piece of genuine catch-up history when it is named later.
    let missed = store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "before broad", &root_id))
        .await
        .unwrap()
        .committed()
        .clone();
    let missed_id = missed.message().id.as_str().to_owned();

    store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            &addressing_broad("are you there"),
            &root_id,
        ))
        .await
        .unwrap();
    let observe_after_first: Vec<(String, String)> = fixture
        .queued_for(BROAD_AGENT_ID)
        .await
        .into_iter()
        .filter(|(_, kind)| kind == "observe")
        .collect();

    store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            &addressing_broad("again"),
            &root_id,
        ))
        .await
        .unwrap();
    let observe_after_second: Vec<(String, String)> = fixture
        .queued_for(BROAD_AGENT_ID)
        .await
        .into_iter()
        .filter(|(_, kind)| kind == "observe")
        .collect();

    // The catch-up covered exactly the one reply broad missed — the root is not here
    // (it is held as a live `turn`), and the second naming added no second copy.
    assert_eq!(observe_after_first, vec![(missed_id, "observe".to_owned())]);
    assert_eq!(observe_after_second, observe_after_first);
}

/// A newly followed agent is caught up with only the most recent replies.
#[tokio::test]
async fn a_followed_cluster_backfills_only_its_most_recent_replies() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();
    let mut replies = Vec::new();
    for n in 0..12 {
        let reply = store
            .send_message(reply_to(
                user_actor(),
                CHANNEL_ID,
                &format!("reply {n}"),
                &root_id,
            ))
            .await
            .unwrap()
            .committed()
            .clone();
        replies.push(reply.message().id.as_str().to_owned());
    }
    store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            &addressing_broad("look"),
            &root_id,
        ))
        .await
        .unwrap();
    let observed: Vec<String> = fixture
        .queued_for(BROAD_AGENT_ID)
        .await
        .into_iter()
        .filter(|(_, kind)| kind == "observe")
        .map(|(id, _)| id)
        .collect();
    assert_eq!(observed, replies[2..]);
}

/// Following is not a way into a conversation: a reference to someone who does not
/// participate widens nothing.
#[tokio::test]
async fn a_reference_to_a_non_participant_follows_nobody() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            "<silverhand://agent/outsider> see this",
            &root_id,
        ))
        .await
        .unwrap();

    assert!(
        !fixture
            .followers(&root_id)
            .await
            .iter()
            .any(|(_, id, _)| id == OUTSIDER_AGENT_ID)
    );
    assert!(fixture.queued_for(OUTSIDER_AGENT_ID).await.is_empty());
}

/// A reference states which namespace answers for its handle, so a human
/// reference must not resolve to an agent that happens to share the name.
#[tokio::test]
async fn a_reference_resolves_only_in_the_namespace_its_kind_names() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            "<silverhand://human/broad> a person by that name?",
            &root_id,
        ))
        .await
        .unwrap();

    assert!(
        !fixture
            .followers(&root_id)
            .await
            .iter()
            .any(|(_, id, _)| id == BROAD_AGENT_ID),
        "a human reference followed an agent of the same name"
    );
}

/// The catch-up is about what a follower has not seen, so its own messages are not
/// queued back to it.
#[tokio::test]
async fn the_catch_up_skips_the_new_followers_own_messages() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let broad = SendActor::Agent(AgentId::parse(BROAD_AGENT_ID).unwrap());
    let root = store
        .send_message(send(broad, CHANNEL_ID, "broad speaks first"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    // The reply makes the root's author a follower, and the only history there is
    // belongs to that author — so the catch-up has nothing to queue.
    let reply = store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            "answering broad",
            &root_id,
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    assert!(fixture.followers(&root_id).await.contains(&(
        "agent".to_owned(),
        BROAD_AGENT_ID.to_owned(),
        "target_author".to_owned()
    )));
    // Its own root is never queued back to it as catch-up...
    assert!(
        !fixture
            .queued_for(BROAD_AGENT_ID)
            .await
            .iter()
            .any(|(id, _)| id == root_id.as_str()),
        "an agent was queued its own message as catch-up"
    );
    // ...but the reply into the cluster it now follows is delivered as a turn.
    assert_eq!(
        fixture.queued_for(BROAD_AGENT_ID).await,
        vec![(reply.message().id.as_str().to_owned(), "turn".to_owned())]
    );
}

/// Who the reply is for, read while the writer is held so it includes the
/// followers this reply created — and never the sender.
#[tokio::test]
async fn the_audience_is_the_cluster_followers_minus_the_sender() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    let by_human = store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            &addressing_broad("and lio"),
            &root_id,
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    // The human's reply queued lio a `turn`, so lio's own reply would trip the
    // freshness guard; the guard is not what this test is about, so it bypasses.
    let by_agent = store
        .send_message(reply_bypassing_freshness(
            agent_actor(),
            CHANNEL_ID,
            "here",
            &root_id,
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    let after_human: Vec<&str> = reply_audience(&by_human);
    let after_agent: Vec<&str> = reply_audience(&by_agent);

    // A person is not in an agent audience, so nothing is excluded for their send.
    assert_eq!(after_human, vec![AGENT_ID, BROAD_AGENT_ID]);
    // The agent that sent this one is.
    assert_eq!(after_agent, vec![BROAD_AGENT_ID]);
    let _ = fixture.latest_seq(CHANNEL_ID).await;
}

/// A cluster's durable consequences share the reply's transaction: if the reply
/// does not commit, nobody was left following a conversation they cannot see.
#[tokio::test]
async fn a_failed_reply_leaves_no_follower_and_no_queued_history() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();
    // The catch-up is the last durable consequence of the reply, so failing it is
    // how the shared transaction is put to the test.
    fixture
        .execute(
            "CREATE TRIGGER reject_test_catch_up BEFORE INSERT ON conversation_inboxes BEGIN \
             SELECT RAISE(ABORT, 'forced catch-up failure'); END",
        )
        .await;

    let error = store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            &addressing_broad("look"),
            &root_id,
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Persistence(_)));
    assert_eq!(
        fixture.followers(&root_id).await,
        vec![],
        "a follower survived a reply that never committed"
    );
    // Only the root's own witness survives — the committed send that ran before the
    // trigger. The failed reply queued broad nothing: no catch-up, no turn.
    assert_eq!(
        fixture.queued_for(BROAD_AGENT_ID).await,
        vec![(root_id.clone(), "turn".to_owned())]
    );
    // The reply itself is gone with them, and so is the ordinal it took.
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 1);
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 1);
}

/// A body that names nobody addresses nobody, end to end: the same handle written
/// as prose rather than as a destination creates no follower and reaches no
/// audience. The prepared command cannot say otherwise, because there is no way to
/// give it a reference the body does not contain.
#[tokio::test]
async fn a_plain_handle_in_prose_addresses_nobody() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    let fact = store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            "broad, look at this",
            &root_id,
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    assert!(
        !fixture
            .followers(&root_id)
            .await
            .iter()
            .any(|(_, id, _)| id == BROAD_AGENT_ID)
    );
    let audience: Vec<&str> = reply_audience(&fact);
    assert_eq!(audience, vec![AGENT_ID]);
}

/// Plants a follower row the send path would never write, so the audience read can
/// be asked about a candidate the write gate refuses.
async fn plant_follower(fixture: &mut Fixture, root: &str, conversation: &str, agent: &str) {
    sqlx::query(
        "INSERT INTO thread_followers (thread_root_id, conversation_id, principal_kind, \
         principal_id, followed_at, source) VALUES (?, ?, 'agent', ?, 1, 'manual')",
    )
    .bind(root)
    .bind(conversation)
    .bind(agent)
    .execute(&mut fixture.db)
    .await
    .unwrap();
}

/// The cluster audience of a reply, read through the view that only a reply has.
fn reply_audience(
    fact: &silverhand_backend_core::conversation_messaging::messaging::MessageCreatedFact,
) -> Vec<&str> {
    fact.reply_cluster()
        .expect("a reply carries a cluster plan")
        .audience()
        .recipients()
        .iter()
        .map(AgentId::as_str)
        .collect()
}

async fn audience_of_next_reply(
    store: &MessagingStore,
    conversation: &str,
    root: &str,
) -> Vec<String> {
    let fact = store
        .send_message(reply_to(user_actor(), conversation, "and again", root))
        .await
        .unwrap()
        .committed()
        .clone();
    fact.reply_cluster()
        .expect("a reply carries a cluster plan")
        .audience()
        .recipients()
        .iter()
        .map(|agent| agent.as_str().to_owned())
        .collect()
}

/// The audience read and the write gate answer the same participation question on a
/// workspace channel, cell by cell: a participant is in and is followed, a
/// non-participant is neither even when a follower row names them, and a follower
/// row that outlived its participation is refused by both.
#[tokio::test]
async fn the_audience_and_the_write_gate_agree_on_a_channel() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();
    store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            &addressing_broad("join us"),
            &root_id,
        ))
        .await
        .unwrap();

    // participant, live: followed by the gate, present in the audience.
    assert!(
        fixture
            .followers(&root_id)
            .await
            .iter()
            .any(|(_, id, _)| id == BROAD_AGENT_ID)
    );
    assert_eq!(
        audience_of_next_reply(&store, CHANNEL_ID, &root_id).await,
        vec![AGENT_ID.to_owned(), BROAD_AGENT_ID.to_owned()]
    );

    // not a participant, live: the gate refuses to follow it, and a planted row
    // does not get it into the audience either.
    plant_follower(&mut fixture, &root_id, CHANNEL_ID, OUTSIDER_AGENT_ID).await;
    assert_eq!(
        audience_of_next_reply(&store, CHANNEL_ID, &root_id).await,
        vec![AGENT_ID.to_owned(), BROAD_AGENT_ID.to_owned()],
        "a follower row admitted a non-participant"
    );

    // participation revoked, follower row left behind: refused by both faces.
    fixture
        .execute(
            "DELETE FROM conversation_agents WHERE conversation_id = 'conv-1' AND agent_id = \
             'agent-2'",
        )
        .await;
    assert_eq!(
        audience_of_next_reply(&store, CHANNEL_ID, &root_id).await,
        vec![AGENT_ID.to_owned()],
        "a lingering follower stayed in the audience after losing participation"
    );
    fixture
        .execute("DELETE FROM thread_followers WHERE principal_id = 'agent-2'")
        .await;
    store
        .send_message(reply_to(
            user_actor(),
            CHANNEL_ID,
            &addressing_broad("still there?"),
            &root_id,
        ))
        .await
        .unwrap();
    assert!(
        !fixture
            .followers(&root_id)
            .await
            .iter()
            .any(|(_, id, _)| id == BROAD_AGENT_ID),
        "the gate followed an agent that no longer participates"
    );
}

/// The same agreement on a direct pair, where participation is named by the row
/// rather than by a roster: the endpoint agent is in without any roster row, and a
/// planted follower that is not an endpoint stays out.
#[tokio::test]
async fn the_audience_and_the_write_gate_agree_on_a_direct_pair() {
    let mut fixture = Fixture::with_scope("agent").await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), PAIR_ID, "in the pair"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    let audience = audience_of_next_reply(&store, PAIR_ID, &root_id).await;
    assert_eq!(audience, vec![AGENT_ID.to_owned()]);
    assert!(
        fixture
            .followers(&root_id)
            .await
            .iter()
            .any(|(kind, id, _)| kind == "agent" && id == AGENT_ID)
    );

    plant_follower(&mut fixture, &root_id, PAIR_ID, OUTSIDER_AGENT_ID).await;
    assert_eq!(
        audience_of_next_reply(&store, PAIR_ID, &root_id).await,
        vec![AGENT_ID.to_owned()],
        "a planted follower entered a pair it is not an endpoint of"
    );
}

impl Fixture {
    async fn direct_pairs(&mut self) -> Vec<(String, String, String)> {
        sqlx::query(
            "SELECT id, endpoint_a_id, endpoint_b_id FROM conversations WHERE placement_kind = \
             'direct_pair' AND id != 'pair-1' ORDER BY id",
        )
        .fetch_all(&mut self.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("id"),
                row.get::<String, _>("endpoint_a_id"),
                row.get::<String, _>("endpoint_b_id"),
            )
        })
        .collect()
    }

    /// The identity and moment of each stored notification, in insertion order.
    /// Kept apart from {@link notification_rows} because these two columns are
    /// MINTED by the adapter rather than projected from the send: they are the only
    /// values a returned row could disagree with the table about.
    async fn notification_identities(&mut self) -> Vec<(String, i64)> {
        sqlx::query("SELECT id, created_at FROM notifications ORDER BY rowid")
            .fetch_all(&mut self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.get::<String, _>("id"), row.get::<i64, _>("created_at")))
            .collect()
    }

    async fn notification_rows(&mut self) -> Vec<serde_json::Value> {
        sqlx::query(
            "SELECT kind, user_id, agent_id, workspace_id, conversation_id, message_id, \
             source_workspace_id, source_conversation_id, source_message_id, payload FROM \
             notifications ORDER BY rowid",
        )
        .fetch_all(&mut self.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "kind": row.get::<String, _>("kind"),
                "user_id": row.get::<String, _>("user_id"),
                // NULL for a `mention_self` row: that kind names a person, not an agent.
                "agent_id": row.get::<Option<String>, _>("agent_id"),
                "workspace_id": row.get::<String, _>("workspace_id"),
                "conversation_id": row.get::<String, _>("conversation_id"),
                "message_id": row.get::<String, _>("message_id"),
                "source_workspace_id": row.get::<String, _>("source_workspace_id"),
                "source_conversation_id": row.get::<String, _>("source_conversation_id"),
                "source_message_id": row.get::<String, _>("source_message_id"),
                "payload": serde_json::from_str::<serde_json::Value>(
                    &row.get::<String, _>("payload"),
                )
                .unwrap(),
            })
        })
        .collect()
    }
}

fn skip_reasons(
    fact: &silverhand_backend_core::conversation_messaging::messaging::MessageCreatedFact,
) -> Vec<String> {
    fact.mention_copies()
        .iter()
        .map(|outcome| match outcome {
            MentionCopyOutcome::Copied { handle, .. } => format!("copied:{}", handle.as_str()),
            MentionCopyOutcome::Skipped { handle, reason, .. } => {
                format!("{reason:?}:{}", handle.as_str())
            }
        })
        .collect()
}

/// Exercise the production authority, rather than the permissive Grants fixture.
/// Scope is chosen at birth; both targets are visible through the same creator.
#[tokio::test]
async fn production_mention_authority_only_copies_agent_scoped_targets() {
    for scope in ["conversation", "agent"] {
        let mut fixture = Fixture::new().await;
        sqlx::query(
            "INSERT INTO agents (id,name,runtime,host_id,created_at,updated_at,display_name, \
             created_by_user_id,session_scope) VALUES \
             ('scope-target','scope-target','claude',?,0,0,'Scope target',?,?)",
        )
        .bind(HOST_ID)
        .bind(USER_ID)
        .bind(scope)
        .execute(&mut fixture.db)
        .await
        .unwrap();
        let store = fixture.store().await;

        for _ in 0..2 {
            let command = SendMessage::prepare(
                draft(
                    user_actor(),
                    CHANNEL_ID,
                    "<silverhand://agent/scope-target> hello",
                ),
                &EveryBracketedRun,
                &store,
            )
            .await
            .unwrap();
            let result = store.send_message(command).await.unwrap();
            assert_eq!(
                skip_reasons(result.committed()),
                vec![if scope == "agent" {
                    "copied:scope-target".to_owned()
                } else {
                    "NoDirectLine:scope-target".to_owned()
                }],
            );
        }

        let expected = if scope == "agent" { 2 } else { 0 };
        assert_eq!(
            fixture.direct_pairs().await.len(),
            usize::from(expected > 0)
        );
        assert_eq!(fixture.queued_for("scope-target").await.len(), expected);
        assert_eq!(fixture.notification_rows().await.len(), expected);
        assert_eq!(
            fixture
                .count("SELECT count(*) FROM messages WHERE cc_source_message_id IS NOT NULL")
                .await,
            expected as i64,
        );
        assert_eq!(
            fixture.count("SELECT count(*) FROM messages WHERE cc_source_message_id IS NOT NULL AND thread_root_id IS NOT NULL").await,
            0,
        );

        // Source removal does not retract a separately delivered DM snapshot.
        sqlx::query("DELETE FROM messages WHERE conversation_id = ?")
            .bind(CHANNEL_ID)
            .execute(&mut fixture.db)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .count("SELECT count(*) FROM messages WHERE cc_source_message_id IS NOT NULL")
                .await,
            expected as i64,
        );
    }
}

/// A mention of someone who cannot read the conversation lands as a copy on the
/// direct line between them and the author — minted with the copy — and the four
/// facts commit together.
#[tokio::test]
async fn a_mention_of_a_non_member_is_copied_onto_a_direct_line() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;

    let fact = store
        .send_message(prepared_with(
            mentioning_outsider(CHANNEL_ID),
            &grants_outsider(),
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    let pairs = fixture.direct_pairs().await;
    assert_eq!(pairs.len(), 1, "the copy did not mint one line: {pairs:?}");
    let (line, endpoint_a, endpoint_b) = pairs.into_iter().next().unwrap();
    // Canonical order: `agent` sorts before `human`.
    assert_eq!(endpoint_a, OUTSIDER_AGENT_ID);
    assert_eq!(endpoint_b, USER_ID);

    let copy = sqlx::query(
        "SELECT id, conversation_id, from_kind, content, metadata, seq, cc_source_message_id FROM \
         messages WHERE cc_source_message_id IS NOT NULL",
    )
    .fetch_one(&mut fixture.db)
    .await
    .unwrap();
    assert_eq!(copy.get::<String, _>("conversation_id"), line);
    assert_eq!(copy.get::<String, _>("from_kind"), "system");
    assert_eq!(copy.get::<i64, _>("seq"), 1);
    assert_eq!(
        copy.get::<String, _>("cc_source_message_id"),
        fact.message().id.as_str()
    );
    assert_eq!(
        copy.get::<String, _>("content"),
        render_copy_body(&MentionCopySource {
            recipient_handle: &ParticipantHandle::parse("outsider").unwrap(),
            source_workspace_name: "Space",
            source_conversation_title: "Channel",
            source_content: fact.message().content.as_str(),
        })
    );
    // The whole stored object, so a missing or extra fact fails rather than passing
    // whatever the spot checks did not look at.
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&copy.get::<String, _>("metadata")).unwrap(),
        serde_json::json!({
            "subtype": "mention_cc",
            "source_workspace_id": WORKSPACE_ID,
            "source_workspace_name": "Space",
            "source_conversation_id": CHANNEL_ID,
            "source_conversation_title": "Channel",
            "source_message_id": fact.message().id.as_str(),
            "source_from_kind": "user",
            "source_from_user_id": USER_ID,
            "source_message_content": fact.message().content.as_str(),
        })
    );

    // The line carries the copy the way any conversation carries a message.
    assert_eq!(fixture.latest_seq(&line).await, 1);
    assert_eq!(fixture.stream_head(&line, None).await, Some(1));

    // The durable half of delivery, and it is a turn rather than context.
    assert_eq!(
        fixture.queued_for(OUTSIDER_AGENT_ID).await,
        vec![(copy.get::<String, _>("id"), "turn".to_owned())]
    );

    // The author's own record points at the message THEY can read — every
    // coordinate is the source's, and the payload carries exactly three facts.
    assert_eq!(
        fixture.notification_rows().await,
        vec![serde_json::json!({
            "kind": "mention_external",
            "user_id": USER_ID,
            "agent_id": OUTSIDER_AGENT_ID,
            "workspace_id": WORKSPACE_ID,
            "conversation_id": CHANNEL_ID,
            "message_id": fact.message().id.as_str(),
            "source_workspace_id": WORKSPACE_ID,
            "source_conversation_id": CHANNEL_ID,
            "source_message_id": fact.message().id.as_str(),
            "payload": {
                "source_workspace_name": "Space",
                "source_conversation_title": "Channel",
                "mention_excerpt": "<silverhand://agent/outsider> please look",
            },
        })]
    );

    // The reported fact IS what was committed: no reread, no second rendering.
    let MentionCopyOutcome::Copied {
        handle,
        agent,
        copy: committed,
    } = &fact.mention_copies()[0]
    else {
        panic!("expected a copy, got {:?}", fact.mention_copies());
    };
    assert_eq!(handle.as_str(), "outsider");
    assert_eq!(agent.as_str(), OUTSIDER_AGENT_ID);
    assert_eq!(committed.id.as_str(), copy.get::<String, _>("id"));
    assert_eq!(committed.conversation_id.as_str(), line);
    // The stored column and the fact are the same authority: the row was written
    // from the value, not from a constant that happens to agree with it.
    assert_eq!(committed.from_kind, FromKind::System);
    assert_eq!(copy.get::<String, _>("from_kind"), "system");
    assert_eq!(committed.content, copy.get::<String, _>("content"));
    assert_eq!(committed.seq, MessageSeq::try_from(1).unwrap());
    assert_eq!(committed.created_at_ms, fact.message().created_at_ms);
    assert_eq!(committed.cc_source_message_id, fact.message().id);
    assert_eq!(committed.metadata.source_workspace_name, "Space");
    assert_eq!(committed.metadata.source_conversation_title, "Channel");
    assert_eq!(committed.metadata.source_from_kind, FromKind::User);
}

#[tokio::test]
async fn a_second_mention_travels_on_the_same_line() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;

    store
        .send_message(prepared_with(
            mentioning_outsider(CHANNEL_ID),
            &grants_outsider(),
        ))
        .await
        .unwrap();
    store
        .send_message(prepared_with(
            mentioning_outsider(CHANNEL_ID),
            &grants_outsider(),
        ))
        .await
        .unwrap();

    let pairs = fixture.direct_pairs().await;
    assert_eq!(pairs.len(), 1);
    let line = pairs.into_iter().next().unwrap().0;
    // Reusing the line still writes all three facts of a send into it.
    let seqs: Vec<i64> = sqlx::query_scalar(
        "SELECT seq FROM messages WHERE cc_source_message_id IS NOT NULL ORDER BY seq",
    )
    .fetch_all(&mut fixture.db)
    .await
    .unwrap();
    assert_eq!(seqs, vec![1, 2]);
    assert_eq!(fixture.latest_seq(&line).await, 2);
    assert_eq!(fixture.stream_head(&line, None).await, Some(2));
    assert_eq!(fixture.queued_for(OUTSIDER_AGENT_ID).await.len(), 2);
}

/// Each reason is decided in an order where the next question only makes sense once
/// the previous is answered, and each is reported per handle.
#[tokio::test]
async fn a_copy_that_does_not_happen_says_why() {
    let mut fixture = Fixture::with_scope("agent").await;
    let store = fixture.store().await;

    // A member already received the original.
    let member = store
        .send_message(prepared_with(
            draft(user_actor(), CHANNEL_ID, &addressing_broad("hello")),
            &Grants(vec![("broad", BROAD_AGENT_ID)]),
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    // Nobody by that name.
    let unknown = store
        .send_message(prepared_with(
            draft(user_actor(), CHANNEL_ID, "<silverhand://agent/ghost> hello"),
            &Grants(vec![]),
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    // The same agent named twice is one addressee, so it is copied once.
    let twice = store
        .send_message(prepared_with(
            draft(
                user_actor(),
                CHANNEL_ID,
                "<silverhand://agent/outsider> and <silverhand://agent/outsider> again",
            ),
            &grants_outsider(),
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    // No line the author may open.
    let ungranted = store
        .send_message(prepared_with(
            mentioning_outsider(CHANNEL_ID),
            &Grants(vec![]),
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    // A direct line has no workspace to name a copy after.
    let from_a_line = store
        .send_message(prepared_with(
            mentioning_outsider(PAIR_ID),
            &grants_outsider(),
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    assert_eq!(
        skip_reasons(&member),
        vec!["AlreadyMember:broad".to_owned()]
    );
    assert_eq!(
        skip_reasons(&unknown),
        vec!["UnknownHandle:ghost".to_owned()]
    );
    assert_eq!(skip_reasons(&twice), vec!["copied:outsider".to_owned()]);
    assert_eq!(
        skip_reasons(&ungranted),
        vec!["NoDirectLine:outsider".to_owned()]
    );
    assert_eq!(
        skip_reasons(&from_a_line),
        vec!["SourceNotWorkspacePlaced:outsider".to_owned()]
    );
    // Exactly one copy happened across all five sends.
    assert_eq!(
        fixture
            .count("SELECT count(*) FROM messages WHERE cc_source_message_id IS NOT NULL")
            .await,
        1
    );
}

/// The grant is about an id; the handle is resolved again inside the unit. If the
/// name now points somewhere else, the verdict does not follow it.
#[tokio::test]
async fn a_grant_for_another_id_does_not_travel_with_the_handle() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;

    let fact = store
        .send_message(prepared_with(
            mentioning_outsider(CHANNEL_ID),
            // Granted for the agent that is a channel member, not for the one this
            // handle names.
            &Grants(vec![("outsider", BROAD_AGENT_ID)]),
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    assert_eq!(
        skip_reasons(&fact),
        vec!["NoDirectLine:outsider".to_owned()]
    );
    assert_eq!(fixture.direct_pairs().await, vec![]);
    assert_eq!(
        fixture
            .count("SELECT count(*) FROM messages WHERE cc_source_message_id IS NOT NULL")
            .await,
        0
    );
}

/// An agent's mention carries no copy: the promise a copy keeps is a person's.
#[tokio::test]
async fn an_agents_mention_copies_nothing() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;

    let fact = store
        .send_message(prepared_with(
            draft(
                agent_actor(),
                CHANNEL_ID,
                "<silverhand://agent/outsider> look",
            ),
            &grants_outsider(),
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    assert!(fact.mention_copies().is_empty());
    assert_eq!(fixture.direct_pairs().await, vec![]);
}

/// The copy's four writes belong to the send's transaction: fail the last of them
/// and the line, the copy, the delivery row, the notification AND the message that
/// caused them are all absent.
#[tokio::test]
async fn a_failed_notification_rolls_back_the_whole_send() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    fixture
        .execute(
            "CREATE TRIGGER reject_test_notification BEFORE INSERT ON notifications BEGIN SELECT \
             RAISE(ABORT, 'forced notification failure'); END",
        )
        .await;

    let error = store
        .send_message(prepared_with(
            mentioning_outsider(CHANNEL_ID),
            &grants_outsider(),
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, SendMessageError::Persistence(_)));
    assert!(root_cause(&error).contains("forced notification failure"));
    assert_eq!(fixture.direct_pairs().await, vec![]);
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 0);
    assert_eq!(
        fixture
            .count("SELECT count(*) FROM conversation_inboxes")
            .await,
        0
    );
    assert!(fixture.notification_rows().await.is_empty());
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 0);
}

#[derive(Debug, thiserror::Error)]
#[error("every effect was told to fail")]
struct EffectRefused;

/// Ports that cannot do anything. What a committed send owes the world is not what
/// makes it a fact, so none of these failing may take the row back.
struct EverythingFails;

impl DirectLineInvalidation for EverythingFails {
    async fn direct_lines_changed(
        &self,
        _author: &UserId,
        _line: &ConversationId,
    ) -> Result<(), EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

impl MessagePublication for EverythingFails {
    async fn publish_copy(&self, _copy: &CommittedMentionCopy) -> Result<(), EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

impl CopyNotification for EverythingFails {
    async fn notify_author(
        &self,
        _author: &UserId,
        _agent: &AgentId,
        _copy: &CommittedMentionCopy,
    ) -> Result<(), EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

impl RuntimeStart for EverythingFails {
    async fn start(
        &self,
        _conversation: &ConversationId,
        _agent: &AgentId,
    ) -> Result<(), EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

impl RuntimeDelivery for EverythingFails {
    async fn deliver(
        &self,
        _agent: &AgentId,
        _copy: &CommittedMentionCopy,
    ) -> Result<(), EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

impl MentionSkipRecord for EverythingFails {
    async fn record_skip(&self, _skipped: &SkippedMention<'_>) -> Result<(), EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

/// Custody: the durable half of a send belongs to the commit, and the announcement
/// afterwards cannot revoke it. Asserted against the database as it is found on a
/// fresh connection, not against the values the send happened to return.
#[tokio::test]
async fn every_effect_failing_leaves_the_committed_facts_in_place() {
    let mut fixture = Fixture::new().await;
    let fact = fixture
        .store()
        .await
        .send_message(prepared_with(
            mentioning_outsider(CHANNEL_ID),
            &grants_outsider(),
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    let refused = EverythingFails;
    let report = announce_send(
        &fact,
        SendEffects {
            direct_lines: &refused,
            publication: &refused,
            notification: &refused,
            start: &refused,
            delivery: &refused,
            skips: &refused,
        },
    )
    .await;
    assert_eq!(report.failed.len(), 5, "every attempt should have failed");

    // Reopened: a new pool on the same file, so what is asserted is what was
    // committed rather than what is still in memory.
    let reopened = fixture.store().await;
    let line = fixture.direct_pairs().await.into_iter().next().unwrap().0;

    assert_eq!(
        fixture
            .count("SELECT count(*) FROM messages WHERE cc_source_message_id IS NOT NULL")
            .await,
        1
    );
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 2);
    assert_eq!(fixture.queued_for(OUTSIDER_AGENT_ID).await.len(), 1);
    // Load-bearing evidence 5: the current-message `turn` witness this fix added is
    // itself durable and still PENDING after publication, start, and delivery all
    // failed — the exact row (target conversation, sent message, `turn`, all three
    // settlement columns empty) a recovering daemon would go on to claim. Both
    // channel participants (the send's recipients; the user sender is excluded) hold
    // it and nothing else.
    let pending_turn = InboxWitness {
        conversation_id: CHANNEL_ID.to_owned(),
        message_id: fact.message().id.as_str().to_owned(),
        delivery_kind: "turn".to_owned(),
        delivered_at: None,
        failed_at: None,
        in_flight_owner: None,
    };
    assert_eq!(fixture.witness_rows(AGENT_ID).await, vec![pending_turn]);
    let pending_turn_broad = InboxWitness {
        conversation_id: CHANNEL_ID.to_owned(),
        message_id: fact.message().id.as_str().to_owned(),
        delivery_kind: "turn".to_owned(),
        delivered_at: None,
        failed_at: None,
        in_flight_owner: None,
    };
    assert_eq!(
        fixture.witness_rows(BROAD_AGENT_ID).await,
        vec![pending_turn_broad]
    );
    assert_eq!(fixture.notification_rows().await.len(), 1);
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 1);
    assert_eq!(fixture.stream_head(CHANNEL_ID, None).await, Some(1));
    assert_eq!(fixture.latest_seq(&line).await, 1);
    assert_eq!(fixture.stream_head(&line, None).await, Some(1));

    // And the reopened store carries on from those facts rather than from zero.
    let next = reopened
        .send_message(send(user_actor(), CHANNEL_ID, "after the failures"))
        .await
        .unwrap()
        .committed()
        .clone();
    assert_eq!(next.message().seq, MessageSeq::try_from(2).unwrap());
}

/// The plan is the send's own answer to "who does this reach, and who did it name",
/// enumerated inside the unit. An adapter that announces it needs no lookup: every
/// recipient and every mention draft is already here.
#[tokio::test]
async fn a_main_send_plans_who_it_reaches_and_who_it_named() {
    let fixture = Fixture::new().await;

    let fact = fixture
        .store()
        .await
        .send_message(send(
            agent_actor(),
            CHANNEL_ID,
            "<silverhand://human/human> take a look",
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    let view = fact
        .main_line()
        .expect("a main-line send carries a main-line plan");
    let plan = view.delivery();
    // The sender is not among the recipients, and a non-participant never was.
    assert_eq!(
        plan.recipients()
            .iter()
            .map(AgentId::as_str)
            .collect::<Vec<_>>(),
        vec![BROAD_AGENT_ID]
    );
    // An agent writing into its own conversation starts nobody: the runtime that
    // would be started is the one already running to write this.
    assert!(plan.start_eligible().is_empty());

    // The intent carries what the SEND knows: the names the body used and where it
    // happened. Who may be told is decided when the record is written, not here.
    let intent = view.mentions().expect("the body named someone");
    assert_eq!(
        intent
            .handles()
            .iter()
            .map(ParticipantHandle::as_str)
            .collect::<Vec<_>>(),
        vec!["human"]
    );
    assert_eq!(intent.author(), None, "an agent send has no human author");
    assert_eq!(intent.trigger_name(), "Lio");
    assert_eq!(intent.source_conversation().as_str(), CHANNEL_ID);
    assert_eq!(intent.source_conversation_title(), "Channel");
    assert_eq!(intent.source_message(), &fact.message().id);
    assert_eq!(intent.source_workspace().as_str(), WORKSPACE_ID);
    assert_eq!(intent.excerpt(), "<silverhand://human/human> take a look");
}

/// A person who cannot read the conversation was not mentioned in it, and naming
/// yourself is not a mention.
#[tokio::test]
async fn a_mention_intent_says_who_wrote_it_so_the_writer_can_exclude_them() {
    let mut fixture = Fixture::new().await;

    let by_the_named_person = fixture
        .store()
        .await
        .send_message(send(
            user_actor(),
            CHANNEL_ID,
            "<silverhand://human/human> talking to myself",
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    fixture
        .execute("DELETE FROM conversation_users WHERE conversation_id = 'conv-1'")
        .await;
    // The first send queued lio a `turn`, so its own next send would be stale; this
    // test is about the mention intent's author, not the guard.
    let after_removal = fixture
        .store()
        .await
        .send_message(send_bypassing_freshness(
            agent_actor(),
            CHANNEL_ID,
            "<silverhand://human/human> are you there",
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    // Both sends named someone, so both carry an intent: who is entitled to a record
    // is not this layer's answer. What the intent does say is who wrote it, which is
    // how the writer excludes an author who named themselves.
    assert_eq!(
        by_the_named_person
            .main_line()
            .unwrap()
            .mentions()
            .expect("the body named someone")
            .author()
            .map(UserId::as_str),
        Some(USER_ID)
    );
    assert_eq!(
        after_removal
            .main_line()
            .unwrap()
            .mentions()
            .expect("the body named someone")
            .author(),
        None
    );
}

/// The three cases of who a main-line send reaches and whom it may start. They differ
/// in both answers, and the difference is the actor and where they are writing.
#[tokio::test]
async fn a_main_line_plan_answers_by_actor_and_destination() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;

    let by_agent_here = store
        .send_message(send(agent_actor(), CHANNEL_ID, "in my own conversation"))
        .await
        .unwrap()
        .committed()
        .clone();
    let by_agent_elsewhere = store
        .send_message(send_across(
            agent_actor(),
            CHANNEL_ID,
            OTHER_CHANNEL_ID,
            "into another conversation",
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    let by_human = store
        .send_message(send(user_actor(), CHANNEL_ID, "from a person"))
        .await
        .unwrap()
        .committed()
        .clone();

    let names =
        |fact: &silverhand_backend_core::conversation_messaging::messaging::MessageCreatedFact| {
            let view = fact.main_line().expect("a main-line send");
            (
                view.delivery()
                    .recipients()
                    .iter()
                    .map(|agent| agent.as_str().to_owned())
                    .collect::<Vec<_>>(),
                view.delivery()
                    .start_eligible()
                    .iter()
                    .map(|agent| agent.as_str().to_owned())
                    .collect::<Vec<_>>(),
            )
        };

    // An agent in its own conversation: not delivered its own message, and starts
    // nobody — the runtime that would be started is the one writing.
    assert_eq!(
        names(&by_agent_here),
        (vec![BROAD_AGENT_ID.to_owned()], vec![])
    );
    // The same agent writing elsewhere is not the actor there: that conversation's
    // own session is a recipient, and idle recipients are started.
    let (elsewhere_recipients, elsewhere_starts) = names(&by_agent_elsewhere);
    assert_eq!(
        elsewhere_recipients,
        vec![AGENT_ID.to_owned(), BROAD_AGENT_ID.to_owned()]
    );
    assert_eq!(elsewhere_starts, elsewhere_recipients);
    // A person excludes nobody and starts whoever is idle.
    let (human_recipients, human_starts) = names(&by_human);
    assert_eq!(
        human_recipients,
        vec![AGENT_ID.to_owned(), BROAD_AGENT_ID.to_owned()]
    );
    assert_eq!(human_starts, human_recipients);
}

/// Naming someone does not depend on the path: a reply names them too, and its
/// delivery is still the cluster's, not the conversation's.
#[tokio::test]
async fn a_reply_names_people_and_still_delivers_to_its_cluster() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();

    let reply = store
        .send_message(reply_to(
            agent_actor(),
            CHANNEL_ID,
            "<silverhand://human/human> in the cluster",
            root.message().id.as_str(),
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    assert!(reply.main_line().is_none());
    let view = reply.reply_cluster().expect("a reply carries its cluster");
    assert_eq!(
        view.mentions()
            .expect("the reply named someone")
            .handles()
            .iter()
            .map(ParticipantHandle::as_str)
            .collect::<Vec<_>>(),
        vec!["human"]
    );
    // Follower-gated, not the participant enumeration: the cluster's only follower is
    // the author of both messages, who is excluded as the sender — while the channel
    // still has another participant that a main-line send would have reached.
    assert!(
        view.audience().recipients().is_empty(),
        "a participant who follows nothing entered the reply's audience: {:?}",
        view.audience().recipients()
    );
    let main_line = store
        .send_message(send(agent_actor(), CHANNEL_ID, "not a reply"))
        .await
        .unwrap()
        .committed()
        .clone();
    assert_eq!(
        main_line
            .main_line()
            .unwrap()
            .delivery()
            .recipients()
            .iter()
            .map(AgentId::as_str)
            .collect::<Vec<_>>(),
        vec![BROAD_AGENT_ID]
    );
}

/// A record of being named is navigated to by workspace and conversation, so a direct
/// line has nowhere to put one.
#[tokio::test]
async fn naming_someone_in_a_direct_line_records_nothing() {
    let fixture = Fixture::with_scope("agent").await;

    let fact = fixture
        .store()
        .await
        .send_message(send(
            agent_actor(),
            PAIR_ID,
            "<silverhand://human/human> over here",
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    assert!(
        fact.main_line()
            .expect("a main-line send")
            .mentions()
            .is_none(),
        "a direct line produced a mention record no reader could reach"
    );
}

/// Who named someone, as a reader sees it: the display name if it says anything, then
/// the name, then the id.
#[tokio::test]
async fn the_trigger_name_falls_back_through_what_the_sender_has() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;

    let with_display = store
        .send_message(send(
            agent_actor(),
            CHANNEL_ID,
            "<silverhand://human/human> one",
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    fixture
        .execute("UPDATE agents SET display_name = '' WHERE id = 'agent-1'")
        .await;
    let with_name = store
        .send_message(send(
            agent_actor(),
            CHANNEL_ID,
            "<silverhand://human/human> two",
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    fixture
        .execute("UPDATE agents SET display_name = '   ' WHERE id = 'agent-1'")
        .await;
    let with_blank_display = store
        .send_message(send(
            agent_actor(),
            CHANNEL_ID,
            "<silverhand://human/human> three",
        ))
        .await
        .unwrap()
        .committed()
        .clone();

    let trigger =
        |fact: &silverhand_backend_core::conversation_messaging::messaging::MessageCreatedFact| {
            fact.main_line()
                .unwrap()
                .mentions()
                .expect("the body named someone")
                .trigger_name()
                .to_owned()
        };
    assert_eq!(trigger(&with_display), "Lio");
    assert_eq!(trigger(&with_name), "lio");
    // Whitespace says nothing either.
    assert_eq!(trigger(&with_blank_display), "lio");
}

/// A second person on the channel, so "no partial rows" is a claim with more than
/// one row to be partial about.
const SECOND_USER_ID: &str = "user-2";

/// Both people the falsifiers below name, in the order the record writes them.
async fn seed_second_viewer(fixture: &mut Fixture) {
    fixture
        .execute(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-2', \
             'second', 'Second', 1, 1)",
        )
        .await;
    fixture
        .execute(
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ('ws-1', \
             'user-2', 0)",
        )
        .await;
    fixture
        .execute(
            "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES \
             ('conv-1', 'user-2', 0)",
        )
        .await;
}

fn naming_both_people(conversation: &str) -> SendMessage {
    send(
        agent_actor(),
        conversation,
        "<silverhand://human/human> <silverhand://human/second> look at this",
    )
}

/// A clock that never answers twice with the same moment, over real entropy.
///
/// Reading the host clock a second time is normally INVISIBLE — two calls inside
/// one millisecond agree — so a test that wanted to catch it would be asserting on
/// timing. Here every reading steps, which turns "this value was read where it was
/// used" into a mechanical question: a row stamped with one reading and reported
/// with another cannot match.
struct SteppingClock {
    readings: AtomicUsize,
}

impl SteppingClock {
    fn new() -> Self {
        Self {
            readings: AtomicUsize::new(0),
        }
    }
}

impl SendClock for SteppingClock {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        Ok(1_000 + self.readings.fetch_add(1, Ordering::SeqCst) as i64)
    }
}

impl SendEntropy for SteppingClock {
    fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        HostEnvironment.fill(buffer)
    }
}

/// The main-line ports, recording what was asked of them. Only the record port is
/// real: these say whether an attempt happened at all, which is the half the
/// database cannot answer.
#[derive(Default)]
struct MainRecorder {
    calls: std::sync::Mutex<Vec<String>>,
    /// The rows the publication was actually handed. Kept whole, because a
    /// consumer sees this object and never the table it came from — so it is the
    /// only thing that can be compared against the table.
    published: std::sync::Mutex<Vec<CommittedMentionRow>>,
}

impl MainRecorder {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn published_rows(&self) -> Vec<CommittedMentionRow> {
        self.published.lock().unwrap().clone()
    }

    fn note(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

impl CommittedMessagePublication for MainRecorder {
    async fn publish_message(
        &self,
        committed: silverhand_backend_core::conversation_messaging::messaging::CommittedMessageView<
            '_,
        >,
    ) -> Result<(), EffectFailure> {
        self.note(format!("published:{}", committed.message().id.as_str()));
        Ok(())
    }
}

impl ViewerNotification for MainRecorder {
    async fn notify_viewer(&self, mention: &CommittedMentionRow) -> Result<(), EffectFailure> {
        self.note(format!("notified:{}", mention.viewer().as_str()));
        self.published.lock().unwrap().push(mention.clone());
        Ok(())
    }
}

impl UnreadPublication for MainRecorder {
    async fn publish_unread(&self, conversation: &ConversationId) -> Result<(), EffectFailure> {
        self.note(format!("unread:{}", conversation.as_str()));
        Ok(())
    }
}

impl MainDelivery for MainRecorder {
    async fn deliver_to(
        &self,
        recipient: &AgentId,
        _message: &silverhand_backend_core::conversation_messaging::messaging::CreatedMessage,
    ) -> Result<(), EffectFailure> {
        self.note(format!("delivered:{}", recipient.as_str()));
        Ok(())
    }
}

impl ReplyDelivery for MainRecorder {
    async fn deliver_to(
        &self,
        recipient: &AgentId,
        _message: &silverhand_backend_core::conversation_messaging::messaging::CreatedMessage,
    ) -> Result<(), EffectFailure> {
        self.note(format!("reply-delivered:{}", recipient.as_str()));
        Ok(())
    }
}

impl IdleStart for MainRecorder {
    async fn start_if_idle(
        &self,
        recipient: &AgentId,
        _conversation: &ConversationId,
    ) -> Result<(), EffectFailure> {
        self.note(format!("started:{}", recipient.as_str()));
        Ok(())
    }
}

/// Runs a committed main-line send's aftermath with the real record adapter and
/// recording everything else.
async fn announce_with_real_record<E: SendClock + SendEntropy + Send + Sync>(
    fact: &silverhand_backend_core::conversation_messaging::messaging::MessageCreatedFact,
    store: &MessagingStore<E>,
    recorder: &MainRecorder,
) -> silverhand_backend_core::conversation_messaging::messaging::post_commit::EffectReport {
    announce_main_send(
        fact.main_line()
            .expect("a main-line send carries a main-line plan"),
        MainEffects {
            publication: recorder,
            mentions: store,
            viewers: recorder,
            unread: recorder,
            delivery: recorder,
            idle: recorder,
        },
    )
    .await
}

/// Runs a committed reply's aftermath with the real record adapter, recording
/// everything else. The delivery capability is [`ReplyDelivery`], not
/// [`MainDelivery`]: the reply reaches the cluster's followers.
async fn announce_reply_with_real_record<E: SendClock + SendEntropy + Send + Sync>(
    fact: &silverhand_backend_core::conversation_messaging::messaging::MessageCreatedFact,
    store: &MessagingStore<E>,
    recorder: &MainRecorder,
) -> silverhand_backend_core::conversation_messaging::messaging::post_commit::EffectReport {
    announce_reply_send(
        fact.reply_cluster()
            .expect("a reply carries a cluster plan"),
        ReplyEffects {
            publication: recorder,
            mentions: store,
            viewers: recorder,
            unread: recorder,
            delivery: recorder,
        },
    )
    .await
}

/// A committed reply's aftermath, driven from the real fact: the frame is published,
/// unread is republished, and the reply is delivered to the cluster's real
/// followers minus its sender — and to nobody by the main-line fan-out, with no
/// idle start. The audience here is the root's author, whom the replying user does
/// not exclude.
#[tokio::test]
async fn a_committed_reply_is_delivered_to_its_cluster_and_never_starts_the_room() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    // A reply that names nobody, so the mention prefix is empty and the aftermath is
    // exactly publish -> unread -> follower delivery.
    let reply = store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "here", &root_id))
        .await
        .unwrap()
        .committed()
        .clone();
    let message = reply.message().id.as_str();

    let recorder = MainRecorder::default();
    let report = announce_reply_with_real_record(&reply, &store, &recorder).await;

    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        recorder.calls(),
        vec![
            format!("published:{message}"),
            format!("unread:{CHANNEL_ID}"),
            format!("reply-delivered:{AGENT_ID}"),
        ],
        "a reply's aftermath was not publish -> unread -> follower delivery"
    );
    // The two seams that separate a reply from a main-line send: no main-line
    // fan-out, and no idle start.
    let calls = recorder.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("delivered:")),
        "a reply used the main-line fan-out: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("started:")),
        "a reply started the room: {calls:?}"
    );
}
/// coordinate triples, and the payload the Activity surface renders. Only the rows
/// that were written are published, so the two people named produce exactly two
/// notifications and exactly two viewer frames.
///
/// Driven by a clock that steps on every reading, so the identity AND the moment of
/// each row are both mechanically traceable to the one place they were minted.
#[tokio::test]
async fn the_record_writes_a_row_per_entitled_viewer_and_publishes_only_those() {
    let mut fixture = Fixture::new().await;
    seed_second_viewer(&mut fixture).await;
    let store = fixture.store_with(SteppingClock::new()).await;

    let fact = store
        .send_message(naming_both_people(CHANNEL_ID))
        .await
        .unwrap()
        .committed()
        .clone();
    let recorder = MainRecorder::default();
    let report = announce_with_real_record(&fact, &store, &recorder).await;

    assert!(report.failed.is_empty(), "{:?}", report.failed);
    let message = fact.message().id.as_str();
    assert_eq!(
        fixture.notification_rows().await,
        vec![
            serde_json::json!({
                "kind": "mention_self",
                "user_id": USER_ID,
                "agent_id": null,
                "workspace_id": WORKSPACE_ID,
                "conversation_id": CHANNEL_ID,
                "message_id": message,
                "source_workspace_id": WORKSPACE_ID,
                "source_conversation_id": CHANNEL_ID,
                "source_message_id": message,
                "payload": {
                    "conversation_title": "Channel",
                    "trigger_name": "Lio",
                    "mentioned_name": "human",
                    "mention_excerpt": "<silverhand://human/human> <silverhand://human/second> \
                                        look at this",
                },
            }),
            serde_json::json!({
                "kind": "mention_self",
                "user_id": SECOND_USER_ID,
                "agent_id": null,
                "workspace_id": WORKSPACE_ID,
                "conversation_id": CHANNEL_ID,
                "message_id": message,
                "source_workspace_id": WORKSPACE_ID,
                "source_conversation_id": CHANNEL_ID,
                "source_message_id": message,
                "payload": {
                    "conversation_title": "Channel",
                    "trigger_name": "Lio",
                    "mentioned_name": "second",
                    "mention_excerpt": "<silverhand://human/human> <silverhand://human/second> \
                                        look at this",
                },
            }),
        ]
    );
    // The load-bearing order: each named person's row exists before unread state is
    // published, because the badge's directed count reads those rows.
    assert_eq!(
        recorder.calls(),
        vec![
            format!("published:{message}"),
            format!("notified:{USER_ID}"),
            format!("notified:{SECOND_USER_ID}"),
            format!("unread:{CHANNEL_ID}"),
            format!("delivered:{BROAD_AGENT_ID}"),
        ]
    );

    // WHAT WAS RETURNED IS WHAT WAS STORED. The identity and the moment are minted
    // per row inside the adapter, so they are the only values the published object
    // could disagree with the table about — every other field is projected from the
    // intent and would survive a second mint unnoticed. Compared against the two
    // columns no other assertion here reads.
    let published = recorder.published_rows();
    let stored = fixture.notification_identities().await;
    assert_eq!(published.len(), 2);
    assert_eq!(
        published
            .iter()
            .map(|row| (row.id().as_str().to_owned(), row.created_at_ms()))
            .collect::<Vec<_>>(),
        stored,
        "a published row does not carry the identity and moment of the row that was written"
    );
    // And the rest of each published row still answers for the same table row.
    let rows = fixture.notification_rows().await;
    for (published, row) in published.iter().zip(&rows) {
        assert_eq!(published.viewer().as_str(), row["user_id"]);
        assert_eq!(
            published.mentioned_name().as_str(),
            row["payload"]["mentioned_name"]
        );
        assert_eq!(
            published.intent().source_message().as_str(),
            row["message_id"]
        );
        assert_eq!(
            published.intent().source_conversation().as_str(),
            row["source_conversation_id"]
        );
        assert_eq!(
            published.intent().source_workspace().as_str(),
            row["source_workspace_id"]
        );
        assert_eq!(
            published.intent().excerpt(),
            row["payload"]["mention_excerpt"]
        );
        assert_eq!(
            published.intent().trigger_name(),
            row["payload"]["trigger_name"]
        );
    }
}

/// Entitlement is decided WHEN THE RECORD IS WRITTEN, and it is two facts, not one.
/// Revoke either half after the send has committed and the person named is told
/// nothing — while the message that named them stays exactly as it was committed,
/// and everyone's unread state is still published.
#[tokio::test]
async fn a_viewer_who_lost_either_half_of_their_access_is_told_nothing() {
    for revocation in [
        "DELETE FROM conversation_users WHERE conversation_id = 'conv-1' AND user_id = 'user-1'",
        "DELETE FROM workspace_users WHERE workspace_id = 'ws-1' AND user_id = 'user-1'",
    ] {
        let mut fixture = Fixture::new().await;
        let store = fixture.store().await;
        let fact = store
            .send_message(send(
                agent_actor(),
                CHANNEL_ID,
                "<silverhand://human/human> still there?",
            ))
            .await
            .unwrap()
            .committed()
            .clone();
        // The send is durable and the intent already names them. What changes is
        // whether they may still read the conversation it happened in.
        fixture.execute(revocation).await;

        let recorder = MainRecorder::default();
        let report = announce_with_real_record(&fact, &store, &recorder).await;

        // Nobody entitled is not a failure: the record was attempted and answered.
        assert!(
            report.failed.is_empty(),
            "{revocation}: {:?}",
            report.failed
        );
        assert!(
            report
                .attempted
                .iter()
                .any(|(_, step)| *step == EffectStep::MentionedViewerRecorded),
            "{revocation}: the record was never attempted"
        );
        assert!(
            fixture.notification_rows().await.is_empty(),
            "{revocation}: a person who cannot read the conversation was told"
        );
        // No row, so no publication — and unread is still published for the rest.
        let calls = recorder.calls();
        assert!(
            !calls.iter().any(|call| call.starts_with("notified:")),
            "{revocation}: {calls:?}"
        );
        assert!(
            calls.contains(&format!("unread:{CHANNEL_ID}")),
            "{revocation}: {calls:?}"
        );

        // And the send itself is untouched by any of it.
        assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 1);
        assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 1);
        assert_eq!(fixture.stream_head(CHANNEL_ID, None).await, Some(1));
    }
}

/// The batch is atomic and its failure is CUSTODY-NEUTRAL: fail the LAST insert and
/// the first person's row goes too — the record either tells everyone it named or
/// nobody — while the committed send keeps its row, its ordinal and its head, the
/// cause survives, and the rest of the aftermath still runs.
#[tokio::test]
async fn a_failed_insert_leaves_no_partial_record_and_takes_no_send_back() {
    let mut fixture = Fixture::new().await;
    seed_second_viewer(&mut fixture).await;
    let store = fixture.store().await;

    let fact = store
        .send_message(naming_both_people(CHANNEL_ID))
        .await
        .unwrap()
        .committed()
        .clone();
    // Aborts the SECOND row of the batch, so the first one has already been written
    // when the failure arrives.
    fixture
        .execute(
            "CREATE TRIGGER reject_second_mention BEFORE INSERT ON notifications WHEN (SELECT \
             count(*) FROM notifications) >= 1 BEGIN SELECT RAISE(ABORT, 'forced mention \
             failure'); END",
        )
        .await;

    let recorder = MainRecorder::default();
    let report = announce_with_real_record(&fact, &store, &recorder).await;

    let (_, step, failure) = report
        .failed
        .iter()
        .find(|(_, step, _)| *step == EffectStep::MentionedViewerRecorded)
        .expect("the record should have failed");
    assert_eq!(*step, EffectStep::MentionedViewerRecorded);
    assert!(root_cause(failure).contains("forced mention failure"));

    assert!(
        fixture.notification_rows().await.is_empty(),
        "the first person's row outlived the batch it belonged to"
    );
    let calls = recorder.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("notified:")),
        "a row that does not exist was published: {calls:?}"
    );
    // The steps that do not depend on the record are still attempted.
    assert!(calls.contains(&format!("unread:{CHANNEL_ID}")), "{calls:?}");
    assert!(
        calls.contains(&format!("delivered:{BROAD_AGENT_ID}")),
        "{calls:?}"
    );

    // The send is a fact regardless: its row, its ordinal and its head all stand.
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 1);
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 1);
    assert_eq!(fixture.stream_head(CHANNEL_ID, None).await, Some(1));
}

/// Naming yourself is not a mention, and the exclusion is the AUTHOR's — so the
/// other person named in the same body still gets theirs.
#[tokio::test]
async fn a_person_who_named_themselves_gets_no_record_but_their_reader_does() {
    let mut fixture = Fixture::new().await;
    seed_second_viewer(&mut fixture).await;
    let store = fixture.store().await;

    let fact = store
        .send_message(send(
            user_actor(),
            CHANNEL_ID,
            "<silverhand://human/human> <silverhand://human/second> me and you",
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    let recorder = MainRecorder::default();
    let report = announce_with_real_record(&fact, &store, &recorder).await;

    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        fixture
            .notification_rows()
            .await
            .iter()
            .map(|row| row["user_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec![SECOND_USER_ID]
    );
    assert!(
        recorder
            .calls()
            .contains(&format!("notified:{SECOND_USER_ID}"))
    );
    assert!(!recorder.calls().contains(&format!("notified:{USER_ID}")));
}

/// The visibility matrix, whole: a record exists for exactly the people a body
/// NAMED **and** who may still read the conversation it happened in. Three of the
/// four cells write nothing, and each is refused for its own reason — so a change
/// that widened one of them could not hide behind the others.
///
/// The fourth cell (named AND entitled) is
/// `the_record_writes_a_row_per_entitled_viewer_and_publishes_only_those`, which
/// also proves the row that was returned is the row that was stored.
#[tokio::test]
async fn only_a_named_person_who_may_still_read_the_conversation_is_recorded() {
    // ENTITLED BUT NOT NAMED. A channel member reads every message in it; being
    // able to read one is not being addressed by it, and this is the cell that says
    // the record is driven by the body rather than by the roster.
    let mut fixture = Fixture::new().await;
    seed_second_viewer(&mut fixture).await;
    let store = fixture.store().await;
    let addressed_to_one = store
        .send_message(send(
            agent_actor(),
            CHANNEL_ID,
            "<silverhand://human/second> only you",
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    let recorder = MainRecorder::default();
    announce_with_real_record(&addressed_to_one, &store, &recorder).await;
    assert_eq!(
        fixture
            .notification_rows()
            .await
            .iter()
            .map(|row| row["user_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec![SECOND_USER_ID],
        "a member who was not named was recorded anyway"
    );

    // NAMED BUT NEVER ENTITLED — as distinct from having LOST access. This person
    // exists and reads another conversation of the same workspace; they were never
    // on this one, so there is nothing to revoke and still nothing to tell them.
    let mut fixture = Fixture::new().await;
    fixture
        .execute(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES \
             ('user-3', 'elsewhere', 'Elsewhere', 2, 2)",
        )
        .await;
    fixture
        .execute(
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at) VALUES ('ws-1', \
             'user-3', 0)",
        )
        .await;
    fixture
        .execute(
            "INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES \
             ('conv-2', 'user-3', 0)",
        )
        .await;
    let store = fixture.store().await;
    let naming_a_stranger = store
        .send_message(send(
            agent_actor(),
            CHANNEL_ID,
            "<silverhand://human/elsewhere> over here",
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    let recorder = MainRecorder::default();
    let report = announce_with_real_record(&naming_a_stranger, &store, &recorder).await;
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert!(
        fixture.notification_rows().await.is_empty(),
        "a person who reads another conversation was told about this one"
    );

    // NEITHER. A body that names nobody carries no intent at all, so the record is
    // not merely empty — it is never attempted, and there is no step to fail.
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let naming_nobody = store
        .send_message(send(agent_actor(), CHANNEL_ID, "just talking"))
        .await
        .unwrap()
        .committed()
        .clone();
    assert!(naming_nobody.main_line().unwrap().mentions().is_none());
    let recorder = MainRecorder::default();
    let report = announce_with_real_record(&naming_nobody, &store, &recorder).await;
    assert!(
        !report
            .attempted
            .iter()
            .any(|(_, step)| *step == EffectStep::MentionedViewerRecorded),
        "a body that named nobody still asked for a record"
    );
    assert!(fixture.notification_rows().await.is_empty());
}

/// The kind in a reference is answered by the namespace it names, and only the human
/// namespace produces this record. Naming an agent addresses it through its own
/// path — the mention copy — so a `mention_self` row for it would be a second,
/// wrongly-shaped telling of the same fact.
///
/// Both names here resolve, and they resolve to different kinds of principal: the
/// row that appears is the human's, and the agent's absence is not a lookup failure.
#[tokio::test]
async fn naming_an_agent_produces_no_viewer_record_even_beside_a_human() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;

    let fact = store
        .send_message(send(
            agent_actor(),
            CHANNEL_ID,
            "<silverhand://agent/broad> <silverhand://human/human> both of you",
        ))
        .await
        .unwrap()
        .committed()
        .clone();
    let intent = fact
        .main_line()
        .unwrap()
        .mentions()
        .expect("the body named someone");
    // The intent already carries only the human half: the kind is decided where the
    // reference is read, not by the store failing to find an agent in `users`.
    assert_eq!(
        intent
            .handles()
            .iter()
            .map(ParticipantHandle::as_str)
            .collect::<Vec<_>>(),
        vec!["human"]
    );

    let recorder = MainRecorder::default();
    announce_with_real_record(&fact, &store, &recorder).await;
    assert_eq!(
        fixture
            .notification_rows()
            .await
            .iter()
            .map(|row| row["user_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec![USER_ID]
    );
}

// ---------------------------------------------------------------------------
// Thread list entry: the cluster's second projection of the reply fact.
// ---------------------------------------------------------------------------

/// A send actor for the second channel agent, whom `agent_actor` is not.
fn broad_actor() -> SendActor {
    SendActor::Agent(AgentId::parse(BROAD_AGENT_ID).unwrap())
}

/// Inserts a reply row directly, so a projection test can pose an exact set of
/// repliers — identities, snapshots, ordinal order — the real send path cannot
/// reach inside one conversation (a legacy row whose snapshot was never written, a
/// fourth distinct identity beyond this channel's roster). `created_at` tracks `seq`
/// so the row's recency is unambiguous.
#[allow(clippy::too_many_arguments)]
async fn insert_reply_row(
    db: &mut SqliteConnection,
    id: &str,
    root: &str,
    seq: i64,
    from_kind: &str,
    from_agent_id: Option<&str>,
    from_user_id: Option<&str>,
    author_id: Option<&str>,
    author_name: Option<&str>,
    author_display_name: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, from_kind, from_agent_id, from_user_id, \
         content, seq, created_at, thread_root_id, author_id, author_name, author_display_name) \
         VALUES (?, ?, ?, ?, ?, 'reply', ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(CHANNEL_ID)
    .bind(from_kind)
    .bind(from_agent_id)
    .bind(from_user_id)
    .bind(seq)
    .bind(seq)
    .bind(root)
    .bind(author_id)
    .bind(author_name)
    .bind(author_display_name)
    .execute(db)
    .await
    .unwrap();
}

/// A stable label for a resolved author, so a test can assert who a replier is and
/// in what order without reaching through each arm inline.
fn replier_label(author: &MessageAuthor) -> String {
    match author {
        MessageAuthor::Active(ActiveAuthor::Agent(agent)) => {
            format!("agent:{}", agent.id().as_str())
        }
        MessageAuthor::Active(ActiveAuthor::Human(human)) => {
            format!("human:{}", human.id().as_str())
        }
        MessageAuthor::Deleted(deleted) => format!("deleted:{}", deleted.name()),
        MessageAuthor::Unresolved(_) => "unresolved".to_owned(),
    }
}

fn labels(entry: &ThreadListEntry) -> Vec<String> {
    entry.repliers().iter().map(replier_label).collect()
}

/// A replier's *current* display name as the projection resolved it. Distinct from
/// [`replier_label`] (which is rename-invariant kind:id): this is the axis a
/// post-commit rename would move, so a test can tell a frozen committed value from a
/// live reread.
fn replier_display(author: &MessageAuthor) -> String {
    match author {
        MessageAuthor::Active(ActiveAuthor::Agent(agent)) => agent.display_name().to_owned(),
        MessageAuthor::Active(ActiveAuthor::Human(human)) => human.display_name().to_owned(),
        MessageAuthor::Deleted(deleted) => deleted.name().to_owned(),
        MessageAuthor::Unresolved(_) => "unresolved".to_owned(),
    }
}

fn display_labels(entry: &ThreadListEntry) -> Vec<String> {
    entry.repliers().iter().map(replier_display).collect()
}

/// Reply-publication ports that always fail: the analog of `EverythingFails` for the
/// message-publication path, so a committed reply's aftermath can be driven to total
/// failure and the durable half asserted afterwards.
struct ReplyEffectsRefused;

impl CommittedMessagePublication for ReplyEffectsRefused {
    async fn publish_message(
        &self,
        _committed: silverhand_backend_core::conversation_messaging::messaging::CommittedMessageView<'_>,
    ) -> Result<(), EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

impl silverhand_backend_core::conversation_messaging::messaging::post_commit::MentionedViewerRecord
    for ReplyEffectsRefused
{
    async fn record_mentioned_viewers(
        &self,
        _intent: &silverhand_backend_core::conversation_messaging::messaging::post_commit::MentionIntent,
    ) -> Result<Vec<CommittedMentionRow>, EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

impl ViewerNotification for ReplyEffectsRefused {
    async fn notify_viewer(&self, _mention: &CommittedMentionRow) -> Result<(), EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

impl UnreadPublication for ReplyEffectsRefused {
    async fn publish_unread(&self, _conversation: &ConversationId) -> Result<(), EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

impl ReplyDelivery for ReplyEffectsRefused {
    async fn deliver_to(
        &self,
        _recipient: &AgentId,
        _message: &silverhand_backend_core::conversation_messaging::messaging::CreatedMessage,
    ) -> Result<(), EffectFailure> {
        Err(EffectFailure::from_source(EffectRefused))
    }
}

/// A publication that records the list entry the frame carried — reply_count, replier
/// labels, their current display names, and the last-reply id — so a test can assert
/// the value publication actually saw, rather than what the database says at announce
/// time.
#[derive(Default)]
struct RecordingPublication {
    observed: std::sync::Mutex<Option<ObservedEntry>>,
}

/// What the recording publication saw on the frame, on every axis a post-commit
/// mutation could move: the count, the replier set (kind:id), their display names,
/// and the last reply.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservedEntry {
    reply_count: i64,
    repliers: Vec<String>,
    replier_names: Vec<String>,
    last_reply_id: String,
}

impl RecordingPublication {
    fn observed(&self) -> ObservedEntry {
        self.observed
            .lock()
            .unwrap()
            .clone()
            .expect("publication was never called")
    }
}

impl CommittedMessagePublication for RecordingPublication {
    async fn publish_message(
        &self,
        committed: silverhand_backend_core::conversation_messaging::messaging::CommittedMessageView<
            '_,
        >,
    ) -> Result<(), EffectFailure> {
        if let Some(fact) = committed.reply_projection() {
            let entry = fact.list_entry();
            *self.observed.lock().unwrap() = Some(ObservedEntry {
                reply_count: entry.reply_count(),
                repliers: labels(entry),
                replier_names: display_labels(entry),
                last_reply_id: entry.last_reply_message_id().as_str().to_owned(),
            });
        }
        Ok(())
    }
}

/// The remaining reply effects, all accepting, so an announcement runs to completion
/// and the recording publication above can be asserted.
struct AcceptingReplyEffects;

impl silverhand_backend_core::conversation_messaging::messaging::post_commit::MentionedViewerRecord
    for AcceptingReplyEffects
{
    async fn record_mentioned_viewers(
        &self,
        _intent: &silverhand_backend_core::conversation_messaging::messaging::post_commit::MentionIntent,
    ) -> Result<Vec<CommittedMentionRow>, EffectFailure> {
        Ok(Vec::new())
    }
}

impl ViewerNotification for AcceptingReplyEffects {
    async fn notify_viewer(&self, _mention: &CommittedMentionRow) -> Result<(), EffectFailure> {
        Ok(())
    }
}

impl UnreadPublication for AcceptingReplyEffects {
    async fn publish_unread(&self, _conversation: &ConversationId) -> Result<(), EffectFailure> {
        Ok(())
    }
}

impl ReplyDelivery for AcceptingReplyEffects {
    async fn deliver_to(
        &self,
        _recipient: &AgentId,
        _message: &silverhand_backend_core::conversation_messaging::messaging::CreatedMessage,
    ) -> Result<(), EffectFailure> {
        Ok(())
    }
}

/// The entry previews the root and reports the authoritative non-system count. The
/// preview is the raw root content (no trim, no ellipsis); the count and the
/// `last_reply_*` pair exclude a system copy, matching the reply badge.
#[tokio::test]
async fn the_entry_previews_the_root_and_counts_only_non_system_replies() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root_body = "  a spaced root that is not trimmed or normalized in its preview";
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, root_body))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    let reply = store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "one", &root_id))
        .await
        .unwrap()
        .committed()
        .clone();
    let reply_id = reply.message().id.as_str().to_owned();

    // A system copy of the cluster: it is not something a participant said, so it
    // moves neither the count nor the last-reply pointer.
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, from_kind, content, seq, created_at, \
         thread_root_id) VALUES ('sys-copy-000000', ?, 'system', 'copied', 98, 99, ?)",
    )
    .bind(CHANNEL_ID)
    .bind(&root_id)
    .execute(&mut fixture.db)
    .await
    .unwrap();

    let conversation = ConversationId::parse(CHANNEL_ID).unwrap();
    let root_ref = ThreadRootId::parse(&root_id).unwrap();
    let entry = store
        .thread_list_entry_for_root(&conversation, &root_ref)
        .await
        .unwrap()
        .expect("a cluster with a non-system reply has an entry");

    assert_eq!(entry.root_id().as_str(), root_id);
    assert_eq!(entry.root_preview(), thread_root_preview(root_body));
    assert_eq!(entry.root_preview(), root_body); // short enough to be returned whole
    assert_eq!(entry.reply_count(), 1);
    assert_eq!(entry.last_reply_message_id().as_str(), reply_id);
    // The root's author is the live agent; a system copy never becomes the author.
    assert_eq!(
        entry.root_author().map(replier_label),
        Some("agent:agent-1".to_owned())
    );
}

/// A root whose only cluster rows are system copies has nothing a participant said,
/// so there is nothing to place: the entry is absent.
#[tokio::test]
async fn a_cluster_with_no_participant_reply_has_no_entry() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    sqlx::query(
        "INSERT INTO messages (id, conversation_id, from_kind, content, seq, created_at, \
         thread_root_id) VALUES ('sys-copy-000000', ?, 'system', 'copied', 98, 99, ?)",
    )
    .bind(CHANNEL_ID)
    .bind(&root_id)
    .execute(&mut fixture.db)
    .await
    .unwrap();

    let conversation = ConversationId::parse(CHANNEL_ID).unwrap();
    let root_ref = ThreadRootId::parse(&root_id).unwrap();
    assert!(
        store
            .thread_list_entry_for_root(&conversation, &root_ref)
            .await
            .unwrap()
            .is_none()
    );
}

/// A preview whose astral scalar straddles the 160th UTF-16 unit stops at the scalar
/// boundary before it: the entry never carries a lone surrogate, which UTF-8 cannot
/// spell. Every scalar-aligned boundary is unaffected.
#[tokio::test]
async fn the_entry_preview_clamps_an_astral_scalar_at_the_utf16_boundary() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    // 159 ASCII fills to unit 159; the next scalar is 2 UTF-16 units, so a UTF-16
    // slice(0,160) would keep a lone high surrogate. The clamp drops it whole.
    let mut root_body = "a".repeat(THREAD_PREVIEW_MAX_UTF16 - 1);
    root_body.push('😀');
    root_body.push_str(" trailing content past the limit");
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, &root_body))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();
    store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "one", &root_id))
        .await
        .unwrap();

    let conversation = ConversationId::parse(CHANNEL_ID).unwrap();
    let root_ref = ThreadRootId::parse(&root_id).unwrap();
    let entry = store
        .thread_list_entry_for_root(&conversation, &root_ref)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        entry.root_preview(),
        "a".repeat(THREAD_PREVIEW_MAX_UTF16 - 1)
    );
}

/// Repliers are distinct by their snapshot identity, ordered most-recent-first, and
/// the shown set is capped at three while the total is not. One replier who spoke
/// twice appears once, at its most recent position.
#[tokio::test]
async fn repliers_are_distinct_most_recent_first_and_capped_while_the_total_is_not() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    // seq 2..=5 through the real path: agent-1, user-1, agent-1 again, agent-2. Each
    // send queues the others a `turn`, so the repeat senders would be stale; this
    // test is about replier identity, not the guard, so the replies bypass it.
    for actor in [agent_actor(), user_actor(), agent_actor(), broad_actor()] {
        store
            .send_message(reply_bypassing_freshness(actor, CHANNEL_ID, "r", &root_id))
            .await
            .unwrap();
    }
    // A fourth distinct identity beyond this channel's roster, most recent of all.
    insert_reply_row(
        &mut fixture.db,
        "reply-outsider0",
        &root_id,
        6,
        "agent",
        Some(OUTSIDER_AGENT_ID),
        None,
        Some(OUTSIDER_AGENT_ID),
        Some("outsider"),
        Some("Outsider"),
    )
    .await;

    let conversation = ConversationId::parse(CHANNEL_ID).unwrap();
    let root_ref = ThreadRootId::parse(&root_id).unwrap();
    let entry = store
        .thread_list_entry_for_root(&conversation, &root_ref)
        .await
        .unwrap()
        .unwrap();

    // Four distinct identities (agent-1 deduped from two replies); shown set capped
    // at three, most-recent-first; total uncapped.
    assert_eq!(entry.reply_count(), 5);
    assert_eq!(entry.replier_total(), 4);
    assert_eq!(
        labels(&entry),
        vec![
            "agent:agent-3".to_owned(),
            "agent:agent-2".to_owned(),
            "agent:agent-1".to_owned(),
        ]
    );
}

/// A replier deleted after speaking keeps its send-time snapshot identity, so it
/// still renders and still counts — a hard delete nulls the live FK, not the byline.
#[tokio::test]
async fn a_deleted_replier_keeps_its_snapshot_and_still_counts() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();
    // broad holds a `turn` from the root send, so its reply would be stale; this
    // test is about the deleted replier's snapshot, not the guard.
    store
        .send_message(reply_bypassing_freshness(
            broad_actor(),
            CHANNEL_ID,
            "from broad",
            &root_id,
        ))
        .await
        .unwrap();

    // Delete the replying agent: ON DELETE SET NULL nulls its FK, its message keeps
    // the snapshot columns written at send.
    sqlx::query("DELETE FROM agents WHERE id = ?")
        .bind(BROAD_AGENT_ID)
        .execute(&mut fixture.db)
        .await
        .unwrap();

    let conversation = ConversationId::parse(CHANNEL_ID).unwrap();
    let root_ref = ThreadRootId::parse(&root_id).unwrap();
    let entry = store
        .thread_list_entry_for_root(&conversation, &root_ref)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.replier_total(), 1);
    // `name()` reads the stored `author_name` snapshot ("broad"), not the display name.
    assert_eq!(labels(&entry), vec!["deleted:broad".to_owned()]);
}

/// A legacy replier with no stable identity — its FK gone and its snapshot never
/// written — renders nothing and does not aggregate, so it is dropped from BOTH the
/// shown set and the total, leaving only the identifiable replier.
#[tokio::test]
async fn an_unresolved_replier_is_dropped_from_the_shown_set_and_the_total() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();
    store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "identifiable", &root_id))
        .await
        .unwrap();

    // A pre-snapshot deleted agent: no FK, no snapshot -> unresolved.
    insert_reply_row(
        &mut fixture.db,
        "reply-legacy0000",
        &root_id,
        6,
        "agent",
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let conversation = ConversationId::parse(CHANNEL_ID).unwrap();
    let root_ref = ThreadRootId::parse(&root_id).unwrap();
    let entry = store
        .thread_list_entry_for_root(&conversation, &root_ref)
        .await
        .unwrap()
        .unwrap();
    // The count still sees two cluster rows, but only one identifiable replier.
    assert_eq!(entry.reply_count(), 2);
    assert_eq!(entry.replier_total(), 1);
    assert_eq!(labels(&entry), vec!["human:user-1".to_owned()]);
}

/// Custody, the reply analog: the list entry is projected inside the reply's own
/// transaction and carried on the committed fact, so it is fixed at commit — the
/// value publication borrows is asserted here directly — and the durable rows behind
/// it survive the total failure of the announcement afterwards.
#[tokio::test]
async fn a_committed_reply_survives_total_effect_failure_including_its_list_entry() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    let reply = store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "here", &root_id))
        .await
        .unwrap()
        .committed()
        .clone();
    let reply_id = reply.message().id.as_str().to_owned();

    // The entry rode back ON the committed fact — the same value the publication frame
    // borrows through `reply_cluster().published()`. Asserted before the announcement,
    // so this is the committed projection, not anything the aftermath produced.
    let entry = reply
        .thread()
        .expect("a reply carries its cluster projection")
        .list_entry();
    assert_eq!(entry.reply_count(), 1);
    assert_eq!(entry.last_reply_message_id().as_str(), reply_id);
    assert_eq!(labels(entry), vec!["human:user-1".to_owned()]);
    assert_eq!(
        reply
            .reply_cluster()
            .expect("a reply carries a cluster plan")
            .published()
            .reply_projection()
            .expect("a reply frame carries its projection")
            .list_entry(),
        entry,
        "publication borrows the very entry the fact carries"
    );

    // Every reply-publication effect fails; none of them may take the row back.
    let refused = ReplyEffectsRefused;
    let report = announce_reply_send(
        reply
            .reply_cluster()
            .expect("a reply carries a cluster plan"),
        ReplyEffects {
            publication: &refused,
            mentions: &refused,
            viewers: &refused,
            unread: &refused,
            delivery: &refused,
        },
    )
    .await;
    assert!(
        !report.failed.is_empty(),
        "the announcement's effects should have been attempted and failed"
    );

    // Reopened: a new pool on the same file, so this is the committed state — the
    // durable rows the entry was projected from all survived the failed announcement.
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 2);
    assert_eq!(fixture.latest_seq(CHANNEL_ID).await, 2);
    assert_eq!(
        fixture.stream_head(CHANNEL_ID, Some(&root_id)).await,
        Some(2)
    );
    // The cluster followers are durable too: the root author follows the thread it
    // opened ("target_author"), and this reply added its own sender ("reply").
    assert_eq!(
        fixture.followers(&root_id).await,
        vec![
            (
                "agent".to_owned(),
                AGENT_ID.to_owned(),
                "target_author".to_owned()
            ),
            ("human".to_owned(), USER_ID.to_owned(), "reply".to_owned()),
        ]
    );
}

/// The entry publication carries is the one committed WITH the reply, never a
/// post-commit reread. After the send commits, a newer reply is written directly and
/// the live author renamed; a cold reread is first shown to observe all of that
/// (count 2, the newer reply as last, both repliers, the renamed name), so the
/// mutation is proven to reach the projection — then publication is asserted to still
/// see the commit-time entry (count 1, the one replier, its committed name, this reply
/// as last). Structurally, `publish_message` takes only a `CommittedMessageView` and
/// no store handle, so a reread at publish time cannot be written — this pins that
/// against regression.
#[tokio::test]
async fn the_published_entry_is_the_committed_one_not_a_post_commit_reread() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    let reply = store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "here", &root_id))
        .await
        .unwrap()
        .committed()
        .clone();
    let reply_id = reply.message().id.as_str().to_owned();

    // Change the cluster AFTER the commit: a newer participant reply from a second
    // actor, and a rename of the first. A reread would now count two repliers, name
    // both, and point last-reply at the newer row.
    insert_reply_row(
        &mut fixture.db,
        "reply-newer00000",
        &root_id,
        7,
        "agent",
        Some(BROAD_AGENT_ID),
        None,
        Some(BROAD_AGENT_ID),
        Some("broad"),
        Some("Broad"),
    )
    .await;
    sqlx::query("UPDATE users SET display_name = 'Renamed' WHERE id = ?")
        .bind(USER_ID)
        .execute(&mut fixture.db)
        .await
        .unwrap();

    // The mutation is only a meaningful counterexample if a reread would actually
    // observe it; prove that first, so "publication is still old" is a real contrast
    // and not a query that silently missed the new row.
    let conversation = ConversationId::parse(CHANNEL_ID).unwrap();
    let root_ref = ThreadRootId::parse(&root_id).unwrap();
    let counterfactual = store
        .thread_list_entry_for_root(&conversation, &root_ref)
        .await
        .unwrap()
        .expect("the cluster still has an entry after the mutation");
    assert_eq!(
        counterfactual.reply_count(),
        2,
        "a reread now counts both replies"
    );
    assert_eq!(
        counterfactual.last_reply_message_id().as_str(),
        "reply-newer00000",
        "a reread now points at the newer reply"
    );
    assert_eq!(
        labels(&counterfactual),
        vec!["agent:agent-2".to_owned(), "human:user-1".to_owned()],
        "a reread now names both repliers, newest first"
    );
    assert_eq!(
        display_labels(&counterfactual),
        vec!["Broad".to_owned(), "Renamed".to_owned()],
        "a reread reflects the live rename"
    );

    // Announce with a recording publication; the rest accept so it runs to completion.
    let publication = RecordingPublication::default();
    let accepting = AcceptingReplyEffects;
    let report = announce_reply_send(
        reply
            .reply_cluster()
            .expect("a reply carries a cluster plan"),
        ReplyEffects {
            publication: &publication,
            mentions: &accepting,
            viewers: &accepting,
            unread: &accepting,
            delivery: &accepting,
        },
    )
    .await;
    assert!(report.failed.is_empty(), "every accepting effect succeeds");

    // Despite the reread above proving the cluster moved on every axis, publication
    // carries the entry committed WITH the reply — count, replier set, live names, and
    // last reply all frozen at commit time.
    assert_eq!(
        publication.observed(),
        ObservedEntry {
            reply_count: 1,
            repliers: vec!["human:user-1".to_owned()],
            replier_names: vec!["Human".to_owned()],
            last_reply_id: reply_id.clone(),
        },
        "the frame carries the committed entry, not a post-commit reread"
    );
}

/// The in-transaction projection carried on the fact and the read-side cold projection
/// are the same value for a settled cluster: both go through the one
/// `project_list_entry`, so a client that places the thread from a cold read sees
/// exactly what rode the reply's frame.
#[tokio::test]
async fn the_cold_read_matches_the_entry_committed_on_the_fact() {
    let fixture = Fixture::new().await;
    let store = fixture.store().await;
    let root = store
        .send_message(send(agent_actor(), CHANNEL_ID, "root"))
        .await
        .unwrap()
        .committed()
        .clone();
    let root_id = root.message().id.as_str().to_owned();

    let reply = store
        .send_message(reply_to(user_actor(), CHANNEL_ID, "here", &root_id))
        .await
        .unwrap()
        .committed()
        .clone();
    let committed = reply
        .thread()
        .expect("a reply carries its cluster projection")
        .list_entry()
        .clone();

    let conversation = ConversationId::parse(CHANNEL_ID).unwrap();
    let root_ref = ThreadRootId::parse(&root_id).unwrap();
    let cold = store
        .thread_list_entry_for_root(&conversation, &root_ref)
        .await
        .unwrap()
        .expect("a settled cluster has a placeable entry");

    assert_eq!(committed, cold);
}

#[tokio::test]
async fn conversation_scoped_pair_refuses_both_senders_inside_the_write_transaction() {
    let mut fixture = Fixture::new().await;
    let store = fixture.store().await;
    for actor in [agent_actor(), user_actor()] {
        let error = store
            .send_message(send(actor, PAIR_ID, "blocked"))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SendMessageError::Conversation(ConversationRefusal::DirectMessageScope)
        ));
    }
    assert_eq!(fixture.count("SELECT count(*) FROM messages").await, 0);
    assert_eq!(fixture.latest_seq(PAIR_ID).await, 0);
}

#[tokio::test]
async fn client_message_identity_is_sender_scoped_and_request_exact() {
    use silverhand_backend_core::conversation_messaging::messaging::ClientMessageId;
    let mut f = Fixture::new().await;
    let store = f.store_with(HostEnvironment).await;
    let token = ClientMessageId::parse("12345678-1234-4234-8234-123456789abc".into()).unwrap();
    let make = |body: &str, target: &str| {
        prepared(draft(
            SendActor::User(UserId::parse(USER_ID).unwrap()),
            target,
            body,
        ))
        .with_client_message_id(Some(token.clone()))
    };
    let first = store.send_message(make("same", CHANNEL_ID)).await.unwrap();
    let seq = f.latest_seq(CHANNEL_ID).await;
    let retry = store.send_message(make("same", CHANNEL_ID)).await.unwrap();
    assert!(retry.is_replay());
    assert_eq!(first.committed().message(), retry.committed().message());
    assert_eq!(f.latest_seq(CHANNEL_ID).await, seq);
    assert!(matches!(
        store.send_message(make("changed", CHANNEL_ID)).await,
        Err(SendMessageError::ClientMessageConflict)
    ));
    assert!(matches!(
        store.send_message(make("same", OTHER_CHANNEL_ID)).await,
        Err(SendMessageError::ClientMessageConflict)
    ));
    let mut reply = draft(
        SendActor::User(UserId::parse(USER_ID).unwrap()),
        CHANNEL_ID,
        "same",
    );
    reply.destination = SendDestination::Reply(first.committed().message().id.clone());
    assert!(matches!(
        store
            .send_message(prepared(reply.clone()).with_client_message_id(Some(token.clone())))
            .await,
        Err(SendMessageError::ClientMessageConflict)
    ));
    let second_token =
        ClientMessageId::parse("12345678-1234-4234-8234-123456789abd".into()).unwrap();
    let reply = prepared(reply).with_client_message_id(Some(second_token));
    let sent = store.send_message(reply.clone()).await.unwrap();
    let retried = store.send_message(reply).await.unwrap();
    assert_eq!(sent.committed().message(), retried.committed().message());
    assert!(retried.is_replay());
    // A different logical send with the same body is a distinct row.
    let distinct = store
        .send_message(make("same", CHANNEL_ID).with_client_message_id(Some(
            ClientMessageId::parse("12345678-1234-4234-8234-123456789abe".into()).unwrap(),
        )))
        .await
        .unwrap();
    assert_ne!(
        first.committed().message().id,
        distinct.committed().message().id
    );
    // A different authenticated author owns a separate token namespace.
    sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u2','second','Second',0,0)").execute(&mut f.db).await.unwrap();
    sqlx::query("INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES(?,'u2',0)")
        .bind(WORKSPACE_ID)
        .execute(&mut f.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES(?,'u2',0)",
    )
    .bind(CHANNEL_ID)
    .execute(&mut f.db)
    .await
    .unwrap();
    let other = prepared(draft(
        SendActor::User(UserId::parse("u2").unwrap()),
        CHANNEL_ID,
        "same",
    ))
    .with_client_message_id(Some(token));
    assert!(!store.send_message(other).await.unwrap().is_replay());
}

#[tokio::test]
async fn client_message_identity_covers_dm_attachments_and_ends_at_hard_delete() {
    use silverhand_backend_core::conversation_messaging::messaging::ClientMessageId;
    let mut f = Fixture::with_scope("agent").await;
    let store = f.store_with(HostEnvironment).await;
    let token = ClientMessageId::parse("12345678-1234-4234-8234-123456789abc".into()).unwrap();
    let dm = prepared(draft(
        SendActor::User(UserId::parse(USER_ID).unwrap()),
        PAIR_ID,
        "DM",
    ))
    .with_client_message_id(Some(token.clone()));
    let a = store.send_message(dm.clone()).await.unwrap();
    assert_eq!(
        store
            .send_message(dm.clone())
            .await
            .unwrap()
            .committed()
            .message()
            .id,
        a.committed().message().id
    );
    // Tokens live on the message, not an eternal receipt table. Physical deletion
    // deliberately ends the retry guarantee; it never resurrects the old identity.
    sqlx::query("DELETE FROM messages WHERE id=?")
        .bind(a.committed().message().id.as_str())
        .execute(&mut f.db)
        .await
        .unwrap();
    let after_delete = store.send_message(dm).await.unwrap();
    assert_ne!(
        after_delete.committed().message().id,
        a.committed().message().id
    );
    let mut attached = draft(
        SendActor::User(UserId::parse(USER_ID).unwrap()),
        CHANNEL_ID,
        "attachment",
    );
    attached.attachment_refs.push(AttachmentRef {
        id: AttachmentId::parse("att-here").unwrap(),
        label: None,
    });
    let token = ClientMessageId::parse("12345678-1234-4234-8234-123456789abd".into()).unwrap();
    let original = prepared(attached.clone()).with_client_message_id(Some(token.clone()));
    let first = store.send_message(original.clone()).await.unwrap();
    // The message snapshot remains the committed content after the source changes.
    sqlx::query("UPDATE attachments SET filename='changed.txt' WHERE id='att-here'")
        .execute(&mut f.db)
        .await
        .unwrap();
    assert_eq!(
        store
            .send_message(original)
            .await
            .unwrap()
            .committed()
            .message(),
        first.committed().message()
    );
    attached.attachment_refs.clear();
    assert!(matches!(
        store
            .send_message(prepared(attached).with_client_message_id(Some(token)))
            .await,
        Err(SendMessageError::ClientMessageConflict)
    ));
}
