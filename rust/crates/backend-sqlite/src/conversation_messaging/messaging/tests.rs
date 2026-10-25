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

    let followers_bounded = plan
        .iter()
        .any(|step| step.starts_with("SEARCH tf") || step.starts_with("SEARCH thread_followers"));
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
    let store =
        MessagingStore::open_with_busy_timeout(&path, HostEnvironment, Duration::from_millis(25))
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
async fn committed_system_message_ensure_reuses_the_same_custody_writer_without_onboarding_gate() {
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
