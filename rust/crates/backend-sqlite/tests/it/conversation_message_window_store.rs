use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageId,
        collaboration::conversation_message_window::{
            ConversationMessageWindowReadPort, ConversationMessageWindowRequest,
            MessageWindowAnchor, MessageWindowAudience, MessageWindowRefusal,
            MessageWindowStreamRequest,
        },
    },
    foundation::identity::{ConversationId, UserId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use silverhand_backend_sqlite::conversation_messaging::messaging::collaboration::conversation_message_window::ConversationMessageWindowStoreError;
use sqlx::{Connection, Executor};

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for statement in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('viewer','viewer','Viewer',1,1),('outsider','outsider','Outsider',1,1),\
         ('reply-a','reply-a','Reply A',1,1),('reply-b','reply-b','Reply B',1,1)",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES \
         ('w','Workspace','workspace',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('w','viewer',1,'member'),('w','outsider',1,'member')",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,latest_message_seq,created_at,updated_at) VALUES \
         ('c','w','workspace','open','Channel','channel','idle','public',5,1,1)",
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,\
         endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,\
         latest_message_seq,created_at,updated_at) VALUES \
         ('pair',NULL,'direct_pair','fixed','human','reply-a','human','viewer','',NULL,'idle',\
         'private',0,1,1)",
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','viewer',1)",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,metadata,seq,\
         created_at,author_id,author_name,author_display_name) VALUES \
         ('root-1','c','user','viewer','root one',NULL,1,10,'viewer','viewer','Viewer'),\
         ('root-2','c','user','viewer','root two','{\"attachments\":[{\"id\":\"att-1\",\"filename\":\"x.png\",\"size\":12,\"content_type\":\"image/png\"}]}',2,20,'viewer','viewer','Viewer')",
        "INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,metadata,seq,\
         created_at,thread_root_id,author_id,author_name,author_display_name) VALUES \
         ('reply-1','c','user','reply-a',' first   reply ',NULL,3,30,'root-1','reply-a','reply-a','Reply A'),\
         ('reply-system','c','system',NULL,'system reply',NULL,4,40,'root-1',NULL,NULL,NULL),\
         ('reply-2','c','user','reply-b','second reply','{\"attachments\":[{\"id\":\"att-2\",\"filename\":\"note.txt\",\"size\":4,\"content_type\":\"text/plain\",\"label\":\"File #2\"}]}',5,50,'root-1','reply-b','reply-b','Reply B')",
        "INSERT INTO conversation_stream_heads(conversation_id,thread_root_id,latest_seq) VALUES \
         ('c',NULL,2),('c','root-1',5)",
        "INSERT INTO message_reactions(id,message_id,reactor_kind,reactor_id,emoji,created_at) VALUES \
         ('reaction-1','root-2','user','viewer','👍',3),\
         ('reaction-2','root-2','user','outsider','👍',4)",
        "INSERT INTO tasks(id,workspace_id,task_number,conversation_id,title,status,creator_kind,\
         creator_user_id,source_message_id,created_at,board_key) VALUES \
         ('task-1','w',1,'c','Linked task','todo','user','viewer','root-2',1,'ws:w')",
    ] {
        db.execute(statement).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn read(
    path: &std::path::Path,
    viewer: &str,
    request: ConversationMessageWindowRequest,
) -> Result<
    Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_message_window::ConversationMessageWindow,
        MessageWindowRefusal,
    >,
    silverhand_backend_core::foundation::persistence::PersistenceFailure,
>{
    MessagingStore::open(path)
        .await
        .unwrap()
        .conversation_message_window()
        .read(
            &UserId::parse(viewer).unwrap(),
            &ConversationId::parse("c").unwrap(),
            request,
        )
        .await
}

async fn read_conversation(
    path: &std::path::Path,
    viewer: &str,
    conversation: &str,
    request: ConversationMessageWindowRequest,
) -> Result<
    Result<
        silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_message_window::ConversationMessageWindow,
        MessageWindowRefusal,
    >,
    silverhand_backend_core::foundation::persistence::PersistenceFailure,
>{
    MessagingStore::open(path)
        .await
        .unwrap()
        .conversation_message_window()
        .read(
            &UserId::parse(viewer).unwrap(),
            &ConversationId::parse(conversation).unwrap(),
            request,
        )
        .await
}

#[tokio::test]
async fn mainline_thread_public_neutralization_and_sidecars_share_one_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("message-window.sqlite");
    seed(&path).await;
    let conversation = ConversationId::parse("c").unwrap();

    let latest = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        2,
        MessageWindowAnchor::Latest,
    )
    .unwrap();
    let bound = read(&path, "viewer", latest.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&UserId::parse("viewer").unwrap(), &conversation, &latest)
        .unwrap();
    assert_eq!(bound.audience(), MessageWindowAudience::Participating);
    assert_eq!(bound.stream_head().get(), 2);
    assert_eq!(
        bound
            .messages()
            .iter()
            .map(|row| row.projection().id.as_str())
            .collect::<Vec<_>>(),
        ["root-1", "root-2"]
    );
    assert_eq!(bound.roots().len(), 2);
    assert_eq!(bound.roots()[0].root().as_str(), "root-1");
    assert_eq!(bound.roots()[0].count().get(), 2);
    assert_eq!(bound.roots()[0].previews().len(), 2);
    assert_eq!(
        bound.roots()[0].previews()[1]
            .first_attachment_label()
            .unwrap()
            .to_string(),
        "File #2"
    );
    assert_eq!(
        bound.messages()[1]
            .projection()
            .linked_task
            .as_ref()
            .unwrap()
            .id(),
        "task-1"
    );
    assert!(!bound.has_more_before() && !bound.has_more_after());
    assert!(
        bound.messages()[1].projection().reactions[0]
            .aggregate()
            .viewer_reacted()
    );

    let public = read(&path, "outsider", latest.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&UserId::parse("outsider").unwrap(), &conversation, &latest)
        .unwrap();
    assert_eq!(public.audience(), MessageWindowAudience::PublicNonmember);
    assert!(
        !public.messages()[1].projection().reactions[0]
            .aggregate()
            .viewer_reacted()
    );

    let thread = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Thread(MessageId::parse("reply-1").unwrap()),
        10,
        MessageWindowAnchor::Latest,
    )
    .unwrap();
    let thread_page = read(&path, "viewer", thread.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&UserId::parse("viewer").unwrap(), &conversation, &thread)
        .unwrap();
    assert_eq!(thread_page.stream().root().unwrap().as_str(), "root-1");
    assert_eq!(thread_page.stream_head().get(), 5);
    assert_eq!(thread_page.messages().len(), 4);

    let before = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        1,
        MessageWindowAnchor::Before(
            silverhand_backend_core::conversation_messaging::messaging::MessageSeq::try_from(2)
                .unwrap(),
        ),
    )
    .unwrap();
    let before_page = read(&path, "viewer", before.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&UserId::parse("viewer").unwrap(), &conversation, &before)
        .unwrap();
    assert_eq!(before_page.messages()[0].projection().id.as_str(), "root-1");
    assert!(!before_page.has_more_before());
    assert!(before_page.has_more_after());

    let after = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        1,
        MessageWindowAnchor::After(
            silverhand_backend_core::conversation_messaging::messaging::MessageSeq::try_from(1)
                .unwrap(),
        ),
    )
    .unwrap();
    let after_page = read(&path, "viewer", after.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&UserId::parse("viewer").unwrap(), &conversation, &after)
        .unwrap();
    assert_eq!(after_page.messages()[0].projection().id.as_str(), "root-2");
    assert!(after_page.has_more_before());
    assert!(!after_page.has_more_after());

    let around = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        1,
        MessageWindowAnchor::Around(MessageId::parse("root-1").unwrap()),
    )
    .unwrap();
    let around_page = read(&path, "viewer", around.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(&UserId::parse("viewer").unwrap(), &conversation, &around)
        .unwrap();
    assert_eq!(around_page.messages()[0].projection().id.as_str(), "root-1");
    assert!(around_page.has_more_after());

    let pair_request = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        10,
        MessageWindowAnchor::Latest,
    )
    .unwrap();
    let pair = read_conversation(&path, "viewer", "pair", pair_request.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(
            &UserId::parse("viewer").unwrap(),
            &ConversationId::parse("pair").unwrap(),
            &pair_request,
        )
        .unwrap();
    assert!(pair.messages().is_empty());

    let missing = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        10,
        MessageWindowAnchor::Around(MessageId::parse("missing").unwrap()),
    )
    .unwrap();
    assert_eq!(
        read(&path, "viewer", missing).await.unwrap().unwrap_err(),
        MessageWindowRefusal::AnchorNotFound
    );
}

#[tokio::test]
async fn the_reused_reader_rechecks_current_membership_and_public_access() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("window-access-change.sqlite");
    seed(&path).await;
    let reader = MessagingStore::open(&path)
        .await
        .unwrap()
        .conversation_message_window();
    let viewer = UserId::parse("viewer").unwrap();
    let conversation = ConversationId::parse("c").unwrap();
    let request = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        50,
        MessageWindowAnchor::Latest,
    )
    .unwrap();
    assert!(
        reader
            .read(&viewer, &conversation, request.clone())
            .await
            .unwrap()
            .is_ok()
    );
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("UPDATE conversations SET visibility='private' WHERE id='c'")
        .await
        .unwrap();
    db.execute("DELETE FROM conversation_users WHERE conversation_id='c' AND user_id='viewer'")
        .await
        .unwrap();
    assert!(matches!(
        reader
            .read(&viewer, &conversation, request.clone())
            .await
            .unwrap(),
        Err(MessageWindowRefusal::Forbidden)
    ));
    db.execute("UPDATE conversations SET visibility='public' WHERE id='c'")
        .await
        .unwrap();
    assert!(
        reader
            .read(&viewer, &conversation, request.clone())
            .await
            .unwrap()
            .is_ok()
    );
    db.execute("DELETE FROM workspace_users WHERE workspace_id='w' AND user_id='viewer'")
        .await
        .unwrap();
    assert!(matches!(
        reader.read(&viewer, &conversation, request).await.unwrap(),
        Err(MessageWindowRefusal::Forbidden)
    ));
    db.close().await.unwrap();
}

#[tokio::test]
async fn selected_reply_sidecars_are_strictly_decoded_before_projection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("message-window-corrupt.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("UPDATE messages SET metadata='not-json' WHERE id='reply-2'")
        .await
        .unwrap();
    db.close().await.unwrap();
    let latest = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        2,
        MessageWindowAnchor::Latest,
    )
    .unwrap();
    assert!(read(&path, "viewer", latest.clone()).await.is_err());

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("UPDATE messages SET metadata=NULL WHERE id='reply-2'")
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read(&path, "viewer", latest).await.unwrap().is_ok());
}

#[tokio::test]
async fn multiple_reply_roots_keep_counts_replier_dedup_and_last_three_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi-root-window.sqlite");
    seed(&path).await;
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    for seq in 6..=9 {
        sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,thread_root_id,author_id,author_name,author_display_name) VALUES(?,'c','user','reply-a','reply',?,?,'root-2','reply-a','reply-a','Reply A')")
            .bind(format!("root2-reply-{seq}")).bind(seq).bind(seq * 10)
            .execute(&mut db).await.unwrap();
    }
    db.execute("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,author_id,author_name,author_display_name) VALUES('root-empty','c','user','viewer','empty',10,100,'viewer','viewer','Viewer')").await.unwrap();
    db.execute("UPDATE conversations SET latest_message_seq=10 WHERE id='c'")
        .await
        .unwrap();
    db.execute("UPDATE conversation_stream_heads SET latest_seq=10 WHERE conversation_id='c' AND thread_root_id IS NULL").await.unwrap();
    db.close().await.unwrap();
    let request = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        50,
        MessageWindowAnchor::Latest,
    )
    .unwrap();
    let window = read(&path, "viewer", request.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(
            &UserId::parse("viewer").unwrap(),
            &ConversationId::parse("c").unwrap(),
            &request,
        )
        .unwrap();
    let roots = window.roots();
    assert_eq!(roots.len(), 3);
    let first = roots
        .iter()
        .find(|r| r.root().as_str() == "root-1")
        .unwrap();
    assert_eq!(first.count().get(), 2);
    let second = roots
        .iter()
        .find(|r| r.root().as_str() == "root-2")
        .unwrap();
    assert_eq!(second.count().get(), 4);
    assert_eq!(second.previews().len(), 3);
    assert_eq!(second.summary().unwrap().repliers().len(), 1);
    assert_eq!(second.summary().unwrap().replier_total().get(), 1);
    assert_eq!(
        second
            .previews()
            .iter()
            .map(|p| p.seq().get())
            .collect::<Vec<_>>(),
        [7, 8, 9]
    );
    let empty = roots
        .iter()
        .find(|r| r.root().as_str() == "root-empty")
        .unwrap();
    assert_eq!(empty.count().get(), 0);
    assert!(empty.previews().is_empty());
}

#[tokio::test]
async fn linked_task_uses_its_actual_conversation_and_rejects_cross_conversation_drift() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("message-window-linked-task-corrupt.sqlite");
    seed(&path).await;
    let latest = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        2,
        MessageWindowAnchor::Latest,
    )
    .unwrap();

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute(
        "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,\
         status,visibility,latest_message_seq,created_at,updated_at) VALUES \
         ('foreign','w','workspace','open','Foreign','foreign','idle','private',0,1,1)",
    )
    .await
    .unwrap();
    db.execute("DROP TRIGGER trg_tasks_board_coordinates_immutable")
        .await
        .unwrap();
    db.execute("UPDATE tasks SET conversation_id='foreign' WHERE id='task-1'")
        .await
        .unwrap();
    db.close().await.unwrap();

    let error = read(&path, "viewer", latest.clone()).await.unwrap_err();
    let source = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<ConversationMessageWindowStoreError>())
        .expect("message-window failure preserves its named SQLite source");
    assert!(matches!(
        source,
        ConversationMessageWindowStoreError::Projection(
            silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_message_window::MessageWindowCorruption::LinkedTask
        )
    ));

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("UPDATE tasks SET conversation_id='c' WHERE id='task-1'")
        .await
        .unwrap();
    db.close().await.unwrap();

    let repaired = read(&path, "viewer", latest.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(
            &UserId::parse("viewer").unwrap(),
            &ConversationId::parse("c").unwrap(),
            &latest,
        )
        .unwrap();
    assert_eq!(
        repaired.messages()[1]
            .projection()
            .linked_task
            .as_ref()
            .unwrap()
            .id(),
        "task-1"
    );
}

/// A linked `tasks.task_number` past `Number.MAX_SAFE_INTEGER` must fault the read
/// (safe 500) rather than serialise a value the JSON-number wire would round. This
/// behaviour is unchanged by the canonical-vocabulary migration — the previous
/// projection-local `WindowTaskNumber` also rejected it — but it is the load-bearing
/// invariant the swap must preserve, so it is pinned explicitly here.
#[tokio::test]
async fn a_linked_task_number_past_the_javascript_safe_integer_faults_the_window_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("message-window-linked-task-unsafe-number.sqlite");
    seed(&path).await;
    let latest = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        2,
        MessageWindowAnchor::Latest,
    )
    .unwrap();

    // task_number is a board coordinate guarded by an immutability trigger; drop it
    // so the corrupt value can be planted the way a persistence fault would present.
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("DROP TRIGGER trg_tasks_board_coordinates_immutable")
        .await
        .unwrap();
    // Number.MAX_SAFE_INTEGER + 1: the first task_number the wire would round.
    db.execute("UPDATE tasks SET task_number=9007199254740992 WHERE id='task-1'")
        .await
        .unwrap();
    db.close().await.unwrap();

    let error = read(&path, "viewer", latest).await.unwrap_err();
    let source = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<ConversationMessageWindowStoreError>())
        .expect("message-window failure preserves its named SQLite source");
    // Canonical TaskNumber rejects the over-safe value at decode, and the corruption
    // is source-bearing: it names the exact column, not a generic linked-task fault.
    assert!(
        matches!(
            source,
            ConversationMessageWindowStoreError::LinkedTask { field } if *field == "tasks.task_number"
        ),
        "expected tasks.task_number source, got {source:?}"
    );
}

/// A non-canonical stored `tasks.title` is corruption, not a title to project
/// verbatim: `tasks.title` carries no canonicity CHECK, and `TaskAggregate::decode`
/// already faults such a row, so the linked-task projection must agree. (A missing
/// title is unconstructable — `tasks.title` is NOT NULL — so the decoder's Option
/// handling of the LEFT JOIN column faults defensively but has no SQL falsifier.)
#[tokio::test]
async fn a_non_canonical_linked_task_title_faults_the_window_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("message-window-linked-task-title.sqlite");
    seed(&path).await;
    let latest = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        2,
        MessageWindowAnchor::Latest,
    )
    .unwrap();

    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    db.execute("UPDATE tasks SET title='  Linked   task  ' WHERE id='task-1'")
        .await
        .unwrap();
    db.close().await.unwrap();

    let error = read(&path, "viewer", latest.clone()).await.unwrap_err();
    let source = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<ConversationMessageWindowStoreError>())
        .expect("message-window failure preserves its named SQLite source");
    assert!(
        matches!(
            source,
            ConversationMessageWindowStoreError::LinkedTask { field }
                if *field == "tasks.title:non_canonical"
        ),
        "expected tasks.title:non_canonical source, got {source:?}"
    );

    // The canonical form round-trips byte-for-byte after repair.
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("message-window-linked-task-title-ok.sqlite");
    seed(&path).await;
    let latest = ConversationMessageWindowRequest::new(
        MessageWindowStreamRequest::Mainline,
        2,
        MessageWindowAnchor::Latest,
    )
    .unwrap();
    let bound = read(&path, "viewer", latest.clone())
        .await
        .unwrap()
        .unwrap()
        .bind(
            &UserId::parse("viewer").unwrap(),
            &ConversationId::parse("c").unwrap(),
            &latest,
        )
        .unwrap();
    assert_eq!(
        bound.messages()[1]
            .projection()
            .linked_task
            .as_ref()
            .unwrap()
            .title(),
        "Linked task"
    );
}

/// `Number.MAX_SAFE_INTEGER` itself and every one of the five task statuses survive
/// the round trip through the canonical `TaskNumber`/`TaskStatus` window projection.
#[tokio::test]
async fn the_max_safe_task_number_and_every_status_round_trip_through_the_window_projection() {
    for status in ["todo", "in_progress", "in_review", "done", "closed"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("message-window-linked-task-roundtrip.sqlite");
        seed(&path).await;
        let latest = ConversationMessageWindowRequest::new(
            MessageWindowStreamRequest::Mainline,
            2,
            MessageWindowAnchor::Latest,
        )
        .unwrap();

        let mut db = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        db.execute("DROP TRIGGER trg_tasks_board_coordinates_immutable")
            .await
            .unwrap();
        let update: &'static str = match status {
            "todo" => {
                "UPDATE tasks SET task_number=9007199254740991, status='todo' WHERE id='task-1'"
            }
            "in_progress" => {
                "UPDATE tasks SET task_number=9007199254740991, status='in_progress' WHERE id='task-1'"
            }
            "in_review" => {
                "UPDATE tasks SET task_number=9007199254740991, status='in_review' WHERE id='task-1'"
            }
            "done" => {
                "UPDATE tasks SET task_number=9007199254740991, status='done' WHERE id='task-1'"
            }
            _ => {
                "UPDATE tasks SET task_number=9007199254740991, status='closed', closed_at=1 WHERE id='task-1'"
            }
        };
        db.execute(update).await.unwrap();
        db.close().await.unwrap();

        let window = read(&path, "viewer", latest.clone())
            .await
            .unwrap()
            .unwrap()
            .bind(
                &UserId::parse("viewer").unwrap(),
                &ConversationId::parse("c").unwrap(),
                &latest,
            )
            .unwrap();
        let linked = window.messages()[1]
            .projection()
            .linked_task
            .as_ref()
            .unwrap();
        assert_eq!(linked.number().get(), 9_007_199_254_740_991);
        assert_eq!(linked.status().as_str(), status);
        assert_eq!(linked.id(), "task-1");
        assert_eq!(linked.title(), "Linked task");
    }
}
