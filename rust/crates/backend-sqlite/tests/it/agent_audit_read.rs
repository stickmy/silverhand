//! Real-adapter falsifiers for the owner-audit read model (#task:2254).
//!
//! The route-level tests (backend-server `identity_access/agent_audit/tests.rs`) pin the
//! two-stage ladder against fake facts; these pin the two properties that only the
//! adapter's own transaction can carry, both flagged in NON-PASS:
//!
//!  * **Gate-before-read zero-reach.** The shared `can_audit_agent_conversation`
//!    verdict is decided IN the Stage-B snapshot BEFORE any message/attachment row is
//!    read, so a denied caller reaches zero data. `other` holds a message and an
//!    attachment row that BOTH fault on decode; `a1` never joined it, so its denied
//!    audit comes back as a clean `Ok(exists,!participates, data = None)` — never the
//!    read fault a read-then-gate ordering would surface as a 500. The control audits
//!    the SAME rows through `a2`, which DOES participate: the gate allows, the rows are
//!    read, and decode faults — proving the denied reads skipped live faulting rows.
//!
//!  * **Stored-coordinate provenance.** An existing conversation's bundle
//!    coordinate is the DECODED `conversations.id`, and an absent conversation's is an
//!    explicit `None` — never the request echo. A cross-conversation attachment id binds
//!    to `None` because the row's own `channel_id` is what seals it.

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AttachmentId,
        collaboration::conversation_message_window::{
            ConversationMessageWindowRequest, MessageWindowAnchor, MessageWindowStreamRequest,
        },
    },
    foundation::identity::{AgentId, ConversationId, Principal, UserId},
    identity_access::agent_audit_read::AgentAuditReadModel,
};
use silverhand_backend_sqlite::identity_access::agent_audit_read::SqliteAgentAuditRead;
use sqlx::Connection;

use crate::current_schema;

fn agent(id: &str) -> AgentId {
    AgentId::parse(id).expect("agent id")
}

fn conversation(id: &str) -> ConversationId {
    ConversationId::parse(id).expect("conversation id")
}

fn attachment(id: &str) -> AttachmentId {
    AttachmentId::parse(id).expect("attachment id")
}

/// The sole owner-auditor of both seeded agents.
fn owner() -> Principal {
    Principal::human(UserId::parse("u-owner").expect("user id"))
}

/// The `mode=all` latest-window request the transcript face issues.
fn latest_window() -> ConversationMessageWindowRequest {
    ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        200,
        MessageWindowAnchor::Latest,
    )
    .expect("legal window request")
}

/// A fresh current-schema database seeded so one `u-owner` audits three agents:
///
///  * `a1` is a member of `joined` — a chat with one valid message and one valid
///    attachment `att-joined`;
///  * `a2` is a member of `other` — a chat carrying ONE corrupt message (`id = ''`),
///    ONE corrupt attachment `att-bad` (`size = -1`), and one valid attachment
///    `att-plain`, and which `a1` never joined. `a2` is ALSO a member of the corrupt
///    workspace chat `''` (an empty conversation id that faults `ConversationId::parse`),
///    so its allowed chat-list read genuinely faults;
///  * `a3` is NON-ACTIVE (`materialization_status = 'failed'`) yet a member of BOTH
///    `other` and the corrupt-`''` chat — so it clears every audit-owner rule
///    (creator + participation) and would read the SAME faulting rows if the Stage-B
///    gate ran only the audit capability. The active-stage visibility floor is the sole
///    gate that stops it.
///
/// So the SAME faulting rows are reached by `a2`'s active, allowed audit and skipped by
/// `a1`'s participation-denied audit AND by `a3`'s visibility-denied audit.
async fn seeded() -> (tempfile::TempDir, SqliteAgentAuditRead) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent-audit-read.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    for statement in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('u-owner','audit-owner','Owner',1,1)",
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h1','Audit Host','local',1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_by_user_id,created_at,updated_at,materialization_status) VALUES \
         ('a1','audit-agent-one','codex','h1','u-owner',1,1,'active'),\
         ('a2','audit-agent-two','codex','h1','u-owner',1,1,'active'),\
         ('a3','audit-agent-three','codex','h1','u-owner',1,1,'failed')",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w1','Audit Workspace','audit-workspace',1,1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,latest_message_seq,created_at,updated_at) VALUES \
         ('joined','w1','workspace','open','Joined','joined','idle','private',1,1,1),\
         ('other','w1','workspace','open','Other','other','idle','private',1,1,1),\
         ('','w1','workspace','open','Corrupt','corrupt','idle','private',1,1,1)",
        // a1 audits only `joined`; a2 and the non-active a3 both join `other`; a2 and a3
        // also join the corrupt-`''` chat so a chat-list read of either would decode it.
        "INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES \
         ('joined','a1',1),('other','a2',1),('other','a3',1),('','a2',1),('','a3',1)",
        // A valid message in the joined chat — a participant read projects it.
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,metadata,seq,\
         created_at,author_id,author_name,author_display_name) VALUES \
         ('m-joined','joined','user','u-owner','hello',NULL,1,10,'u-owner','audit-owner','Owner')",
        // The corrupt message (empty id) in the chat a1 never joined — decode faults iff read.
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,metadata,seq,\
         created_at,author_id,author_name,author_display_name) VALUES \
         ('','other','user','u-owner','corrupt',NULL,1,10,'u-owner','audit-owner','Owner')",
        // A valid attachment bound to joined, plus a valid and a CORRUPT (size = -1)
        // attachment bound to other; the corrupt one faults on decode iff read.
        "INSERT INTO attachments(id,channel_id,source_conversation_id,filename,size,content_type,\
         storage,storage_key,created_by_kind,created_by_id,created_at,content_revision) VALUES \
         ('att-joined','joined','joined','a.png',4,'image/png','local','attachments/joined/a.png','user','u-owner',10,1),\
         ('att-plain','other','other','b.png',4,'image/png','local','attachments/other/b.png','user','u-owner',10,1),\
         ('att-bad','other','other','c.png',-1,'image/png','local','attachments/other/c.png','user','u-owner',10,1)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let adapter = SqliteAgentAuditRead::open(&path).await.unwrap();
    (directory, adapter)
}

// ---- P0#3: stored-coordinate provenance ------------------------------------

#[tokio::test]
async fn transcript_of_a_joined_chat_carries_the_decoded_stored_coordinate() {
    let (_guard, adapter) = seeded().await;
    let bundle = adapter
        .read_audit_transcript(
            &owner(),
            &agent("a1"),
            &conversation("joined"),
            &latest_window(),
        )
        .await
        .expect("read succeeds")
        .expect("agent exists");

    // The coordinate is decoded from the row, the gate allows, and the window is read.
    assert_eq!(bundle.conversation(), Some(&conversation("joined")));
    assert!(bundle.audited().exists);
    assert!(bundle.audited().agent_participates);
    let window = bundle.window().expect("allowed read has a window");
    assert_eq!(window.messages().len(), 1);
}

#[tokio::test]
async fn transcript_of_an_absent_chat_has_no_echoed_coordinate() {
    let (_guard, adapter) = seeded().await;
    let bundle = adapter
        .read_audit_transcript(
            &owner(),
            &agent("a1"),
            &conversation("ghost"),
            &latest_window(),
        )
        .await
        .expect("read succeeds")
        .expect("agent exists");

    // Absence is the explicit `None` arm — NOT the request echo `Some("ghost")`.
    assert_eq!(bundle.conversation(), None);
    assert!(!bundle.audited().exists);
    assert!(bundle.window().is_none());
}

#[tokio::test]
async fn attachment_binds_by_the_rows_own_channel_not_the_requested_conversation() {
    let (_guard, adapter) = seeded().await;
    // `att-plain` is a real row, but its `channel_id` is `other`; auditing it under
    // `joined` (which a1 joined) binds to `None`, so a guessed id is no cross-chat oracle.
    let bundle = adapter
        .read_audit_attachment(
            &owner(),
            &agent("a1"),
            &conversation("joined"),
            &attachment("att-plain"),
        )
        .await
        .expect("read succeeds")
        .expect("agent exists");

    assert_eq!(bundle.conversation(), Some(&conversation("joined")));
    assert!(bundle.audited().agent_participates);
    assert!(bundle.attachment().is_none());
}

#[tokio::test]
async fn attachment_in_the_audited_chat_resolves_its_row() {
    let (_guard, adapter) = seeded().await;
    let bundle = adapter
        .read_audit_attachment(
            &owner(),
            &agent("a1"),
            &conversation("joined"),
            &attachment("att-joined"),
        )
        .await
        .expect("read succeeds")
        .expect("agent exists");

    let record = bundle.attachment().expect("row is bound to this chat");
    assert_eq!(record.channel(), &conversation("joined"));
    assert_eq!(record.id(), &attachment("att-joined"));
}

// ---- P0#2: gate-before-read zero-reach -------------------------------------

#[tokio::test]
async fn denied_owner_never_reads_a_corrupt_transcript_row() {
    let (_guard, adapter) = seeded().await;
    // `other` holds a message whose empty id faults on decode, but `a1` never joined it,
    // so the gate denies BEFORE the window is read: a clean bundle, not a read fault.
    let bundle = adapter
        .read_audit_transcript(
            &owner(),
            &agent("a1"),
            &conversation("other"),
            &latest_window(),
        )
        .await
        .expect("denied read does not touch the corrupt row")
        .expect("agent exists");

    assert_eq!(bundle.conversation(), Some(&conversation("other")));
    assert!(bundle.audited().exists);
    assert!(!bundle.audited().agent_participates);
    assert!(bundle.window().is_none());
}

#[tokio::test]
async fn participant_read_of_the_same_corrupt_transcript_row_genuinely_faults() {
    let (_guard, adapter) = seeded().await;
    // The control: `a2` IS a member of `other`, so the gate allows and the SAME corrupt
    // row IS read — proving the denied read above skipped a live fault, not a benign row.
    let result = adapter
        .read_audit_transcript(
            &owner(),
            &agent("a2"),
            &conversation("other"),
            &latest_window(),
        )
        .await;

    assert!(
        result.is_err(),
        "reading the corrupt row must surface a read fault"
    );
}

#[tokio::test]
async fn denied_owner_never_reads_a_corrupt_attachment_row() {
    let (_guard, adapter) = seeded().await;
    // `att-bad` lives in `other`, which `a1` never joined; the gate denies before the
    // attachment row is loaded, so even a request for it comes back clean.
    let bundle = adapter
        .read_audit_attachment(
            &owner(),
            &agent("a1"),
            &conversation("other"),
            &attachment("att-bad"),
        )
        .await
        .expect("denied read does not touch the row")
        .expect("agent exists");

    assert!(bundle.audited().exists);
    assert!(!bundle.audited().agent_participates);
    assert!(bundle.attachment().is_none());
}

#[tokio::test]
async fn participant_read_of_the_same_corrupt_attachment_row_genuinely_faults() {
    let (_guard, adapter) = seeded().await;
    // The control: `a2` IS a member of `other`, so the gate allows, the SAME corrupt row
    // IS loaded, and its `size = -1` faults on decode.
    let result = adapter
        .read_audit_attachment(
            &owner(),
            &agent("a2"),
            &conversation("other"),
            &attachment("att-bad"),
        )
        .await;

    assert!(
        result.is_err(),
        "reading the corrupt attachment row must surface a read fault"
    );
}

// ---- P0#2 (visibility floor): the active-stage gate runs IN the Stage-B snapshot ------
//
// `a3` clears every audit-owner rule — it is the creator, and it participates in `other`
// and the corrupt-`''` chat — so an in-adapter gate that ran ONLY the audit capability
// would read the SAME faulting rows `a2`'s allowed audit reaches. The ONLY gate that
// stops `a3` is the active-stage `can_view_agent` floor, because `a3` is non-active
// (`materialization_status = 'failed'`). These tests prove that floor is decided in the
// Stage-B transaction, before any product row is read.

#[tokio::test]
async fn non_active_owner_never_reads_a_corrupt_transcript_row() {
    let (_guard, adapter) = seeded().await;
    // `a3` participates in `other`, so the audit-owner rule alone would allow and read the
    // corrupt message. The non-active visibility floor denies first: a clean bundle whose
    // `None` window the route maps to a fresh 404 — never the read fault of the corrupt row.
    let bundle = adapter
        .read_audit_transcript(
            &owner(),
            &agent("a3"),
            &conversation("other"),
            &latest_window(),
        )
        .await
        .expect("non-active read does not touch the corrupt row")
        .expect("agent row exists");

    assert_eq!(bundle.conversation(), Some(&conversation("other")));
    assert!(bundle.audited().exists);
    // Participation is TRUE — the audit-owner rule would have admitted; only the stage
    // floor stopped the read.
    assert!(bundle.audited().agent_participates);
    assert!(bundle.window().is_none());
}

#[tokio::test]
async fn non_active_owner_never_reads_a_corrupt_attachment_row() {
    let (_guard, adapter) = seeded().await;
    // Same shape for the attachment face: `a3` participates in `other`, yet the non-active
    // floor skips the `att-bad` load, so the bundle is clean rather than a decode fault.
    let bundle = adapter
        .read_audit_attachment(
            &owner(),
            &agent("a3"),
            &conversation("other"),
            &attachment("att-bad"),
        )
        .await
        .expect("non-active read does not touch the corrupt row")
        .expect("agent row exists");

    assert!(bundle.audited().exists);
    assert!(bundle.audited().agent_participates);
    assert!(bundle.attachment().is_none());
}

#[tokio::test]
async fn non_active_owner_lists_no_chats_even_over_a_corrupt_summary_row() {
    let (_guard, adapter) = seeded().await;
    // The list face has NO participation gate to accidentally mask the visibility floor:
    // `a3` is the creator, so the audit-owner rule admits unconditionally. It joins the
    // corrupt-`''` chat, whose empty id faults `ConversationId::parse` if decoded. The
    // non-active floor skips the whole chat read, so the bundle is a clean empty list.
    let bundle = adapter
        .read_audit_chats(&owner(), &agent("a3"))
        .await
        .expect("non-active read does not touch the corrupt summary row")
        .expect("agent row exists");

    assert!(bundle.chats().is_empty());
}

#[tokio::test]
async fn active_owner_read_of_the_same_corrupt_summary_row_genuinely_faults() {
    let (_guard, adapter) = seeded().await;
    // The control: `a2` is active and joins the same corrupt-`''` chat, so its allowed
    // chat-list read DOES decode the empty id and faults — proving the non-active read
    // above skipped a live fault, not a benign empty membership.
    let result = adapter.read_audit_chats(&owner(), &agent("a2")).await;

    assert!(
        result.is_err(),
        "reading the corrupt conversation summary must surface a read fault"
    );
}
