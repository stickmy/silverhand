use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::Value;
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::message_unread::{
        ConversationMessageUnread, ConversationMessageUnreadReadPort,
        ConversationMessageUnreadRefusal, LatestUnreadPointer, NonZeroUnreadCount, UnreadCount,
        UnreadRollup, ViewerMessageUnreadSummary, ViewerMessageUnreadSummaryReadPort,
        WorkspaceConversationUnread, WorkspaceMessageUnread, WorkspaceMessageUnreadReadPort,
        WorkspaceMessageUnreadRefusal, WorkspaceUnreadTotal,
    },
    conversation_messaging::messaging::{MessageId, MessageSeq},
    foundation::identity::{ConversationId, UserId, WorkspaceId},
};
use silverhand_backend_server::conversation_messaging::messaging::collaboration::message_unread::router;
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
use tower::ServiceExt;

const SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
));

async fn store(setup: &str) -> (tempfile::TempDir, MessagingStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("message-unread.sqlite");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    db.execute(sqlx::raw_sql(SCHEMA)).await.unwrap();
    sqlx::query("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('local','local-message-unread','Local',1,1),('other','other-message-unread','Other',1,1)")
        .execute(&mut db).await.unwrap();
    if matches!(
        setup,
        "summary_empty" | "missing_workspace" | "missing_conversation"
    ) {
        drop(db);
        return (dir, MessagingStore::open(path).await.unwrap());
    }
    if setup == "direct_pair_endpoint" {
        sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,title_key,status,visibility,created_at,updated_at) VALUES('c',NULL,'direct_pair','fixed','human','local','human','other','',NULL,'idle','private',1,1)")
            .execute(&mut db).await.unwrap();
        seed_messages(&mut db, "c").await;
        drop(db);
        return (dir, MessagingStore::open(path).await.unwrap());
    }

    sqlx::query("INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES('w','Workspace','message-unread',1,1)")
        .execute(&mut db).await.unwrap();
    let archived = (setup == "archived_workspace_member").then_some(9_i64);
    let visibility = if setup == "public_workspace_nonmember" {
        "public"
    } else {
        "private"
    };
    sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,archived_at,created_at,updated_at) VALUES('c','w','workspace','open','General','general-message-unread','idle',?,?,1,1)")
        .bind(visibility).bind(archived).execute(&mut db).await.unwrap();
    if setup != "workspace_forbidden" {
        sqlx::query(
            "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES('w','local',1)",
        )
        .execute(&mut db)
        .await
        .unwrap();
    }
    if setup != "workspace_forbidden" && setup != "public_workspace_nonmember" {
        sqlx::query("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c','local',1)")
            .execute(&mut db).await.unwrap();
        seed_messages(&mut db, "c").await;
    }
    if setup == "workspace_member" {
        sqlx::query("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES('c2','w','workspace','open','Second','second-message-unread','idle','private',2,2)")
            .execute(&mut db).await.unwrap();
        sqlx::query("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c2','local',2)")
            .execute(&mut db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES('c2-message','c2','user','other','other conversation',1,9)")
            .execute(&mut db).await.unwrap();
        sqlx::query("INSERT INTO conversation_stream_heads(conversation_id,thread_root_id,latest_seq) VALUES('c2',NULL,1)")
            .execute(&mut db).await.unwrap();
    }
    drop(db);
    (dir, MessagingStore::open(path).await.unwrap())
}

async fn seed_messages(db: &mut SqliteConnection, conversation: &str) {
    sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at,thread_root_id) VALUES('main-1',?,'user','other','main first',1,1,NULL),('own-main',?,'user','local','own',2,2,NULL),('system-main',?,'system',NULL,'system',3,3,NULL),('root',?,'user','other','root',4,4,NULL),('reply-1',?,'user','other','reply one',5,5,'root'),('own-reply',?,'user','local','own reply',6,6,'root'),('reply-2',?,'user','other','reply two',7,7,'root')")
        .bind(conversation).bind(conversation).bind(conversation).bind(conversation)
        .bind(conversation).bind(conversation).bind(conversation)
        .execute(&mut *db).await.unwrap();
    sqlx::query("INSERT INTO conversation_stream_heads(conversation_id,thread_root_id,latest_seq) VALUES(?,NULL,4),(?,'root',7)")
        .bind(conversation).bind(conversation).execute(&mut *db).await.unwrap();
    sqlx::query("INSERT INTO conversation_reads(user_id,conversation_id,thread_root_id,last_read_seq,read_at) VALUES('local',?,NULL,1,8),('local',?,'root',5,8)")
        .bind(conversation).bind(conversation).execute(&mut *db).await.unwrap();
    sqlx::query("INSERT INTO thread_followers(thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source) VALUES('root',?,'human','local',8,'manual')")
        .bind(conversation).execute(&mut *db).await.unwrap();
}

#[tokio::test]
async fn production_router_matches_the_typescript_signed_closed_set() {
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/message-unread-wire.json"
    )))
    .unwrap();
    let entries = fixture.as_array().unwrap();
    for entry in entries {
        let setup = entry["setup"].as_str().unwrap();
        let (_dir, store) = store(setup).await;
        let mut request = Request::builder()
            .uri(entry["path"].as_str().unwrap())
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = router(store.message_unread())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            entry["status"].as_u64().unwrap() as u16,
            "{}",
            entry["name"]
        );
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body, entry["body"], "{}", entry["name"]);
    }
    assert_eq!(entries.len(), 10);
}

#[tokio::test]
async fn all_three_surfaces_reject_a_missing_human_principal() {
    let (_dir, store) = store("workspace_member").await;
    for path in [
        "/api/workspaces/w/message-unread",
        "/api/message-unread-summary",
        "/api/channels/c/message-unread",
    ] {
        let response = router(store.message_unread())
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}

#[derive(Clone)]
struct SplicedRead {
    summary_viewer: &'static str,
    summary_row_viewer: Option<&'static str>,
}

impl WorkspaceMessageUnreadReadPort for SplicedRead {
    type Error = std::io::Error;

    async fn read_workspace(
        &self,
        _: &UserId,
        _: &WorkspaceId,
    ) -> Result<Result<WorkspaceMessageUnread, WorkspaceMessageUnreadRefusal>, Self::Error> {
        let workspace = WorkspaceId::parse("b").unwrap();
        let conversation = ConversationId::parse("b-conversation").unwrap();
        let unread = UnreadRollup::projected(
            UnreadCount::new(1).unwrap(),
            Some(LatestUnreadPointer::projected(
                conversation.clone(),
                MessageId::parse("m").unwrap(),
                MessageSeq::try_from(1).unwrap(),
                None,
            )),
        )
        .unwrap();
        let UnreadRollup::Unread(unread) = unread else {
            unreachable!()
        };
        Ok(Ok(WorkspaceMessageUnread::projected(
            workspace.clone(),
            vec![WorkspaceConversationUnread::projected(workspace, conversation, unread).unwrap()],
        )
        .unwrap()))
    }
}

impl ViewerMessageUnreadSummaryReadPort for SplicedRead {
    type Error = std::io::Error;

    async fn read_summary(&self, _: &UserId) -> Result<ViewerMessageUnreadSummary, Self::Error> {
        let rows = self
            .summary_row_viewer
            .map(|viewer| {
                WorkspaceUnreadTotal::projected(
                    UserId::parse(viewer).unwrap(),
                    WorkspaceId::parse("w").unwrap(),
                    NonZeroUnreadCount::new(UnreadCount::new(1).unwrap()).unwrap(),
                )
            })
            .into_iter()
            .collect();
        ViewerMessageUnreadSummary::projected(UserId::parse(self.summary_viewer).unwrap(), rows)
            .map_err(std::io::Error::other)
    }
}

impl ConversationMessageUnreadReadPort for SplicedRead {
    type Error = std::io::Error;

    async fn read_conversation(
        &self,
        _: &UserId,
        _: &ConversationId,
    ) -> Result<Result<ConversationMessageUnread, ConversationMessageUnreadRefusal>, Self::Error>
    {
        Ok(Ok(ConversationMessageUnread::projected(
            ConversationId::parse("b").unwrap(),
            UnreadRollup::CaughtUp,
        )
        .unwrap()))
    }
}

#[tokio::test]
async fn request_coordinates_bind_before_either_success_envelope() {
    for path in [
        "/api/workspaces/a/message-unread",
        "/api/channels/a/message-unread",
    ] {
        let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = router(SplicedRead {
            summary_viewer: "local",
            summary_row_viewer: None,
        })
        .oneshot(request)
        .await
        .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body, serde_json::json!({"error":"internal error"}));
    }
}

#[tokio::test]
async fn summary_binds_aggregate_and_every_row_viewer_before_wire() {
    let cases = [
        (
            "other",
            Some("other"),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            "local",
            Some("other"),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        ),
        ("local", Some("local"), axum::http::StatusCode::OK),
        ("local", None, axum::http::StatusCode::OK),
    ];
    for (summary_viewer, summary_row_viewer, expected) in cases {
        let mut request = Request::builder()
            .uri("/api/message-unread-summary")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = router(SplicedRead {
            summary_viewer,
            summary_row_viewer,
        })
        .oneshot(request)
        .await
        .unwrap();
        assert_eq!(response.status(), expected);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        if expected == axum::http::StatusCode::OK {
            let expected_body = if summary_row_viewer.is_some() {
                serde_json::json!({"by_workspace":{"w":1}})
            } else {
                serde_json::json!({"by_workspace":{}})
            };
            assert_eq!(body, expected_body);
        } else {
            assert_eq!(body, serde_json::json!({"error":"internal error"}));
        }
    }
}

#[tokio::test]
async fn aggregate_counts_retain_eligible_corrupt_middle_rows() {
    // Approved aggregate contract: unrelated non-returned fields are not audited.
    // Every mutation remains eligible under the unchanged unread predicate.
    for mutation in [
        "UPDATE messages SET from_user_id='missing-author' WHERE id='own-reply'",
        "UPDATE messages SET from_user_id='other',id='' WHERE id='own-reply'",
        "UPDATE messages SET from_user_id='other',id=NULL WHERE id='own-reply'",
        "UPDATE messages SET from_user_id='other',seq=6.5 WHERE id='own-reply'",
        "UPDATE messages SET from_user_id='other',id=x'61' WHERE id='own-reply'",
        "UPDATE messages SET from_user_id='other',id=CAST(x'80' AS TEXT) WHERE id='own-reply'",
    ] {
        let (dir, store) = store("workspace_member").await;
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(dir.path().join("message-unread.sqlite"))
                .foreign_keys(false),
        )
        .await
        .unwrap();
        db.execute(sqlx::raw_sql(mutation)).await.unwrap();
        for path in [
            "/api/workspaces/w/message-unread",
            "/api/message-unread-summary",
            "/api/channels/c/message-unread",
        ] {
            let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
            request
                .extensions_mut()
                .insert(UserId::parse("local").unwrap());
            let response = router(store.message_unread())
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::OK,
                "{mutation} at {path}"
            );
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            if path == "/api/message-unread-summary" {
                assert_eq!(body, serde_json::json!({"by_workspace":{"w":4}}));
            } else if path == "/api/workspaces/w/message-unread" {
                assert_eq!(body["total"], 4);
                assert_eq!(body["by_conversation"]["c"], 3);
                assert_eq!(body["latest_by_conversation"]["c"]["message_id"], "reply-2");
                assert_eq!(body["latest_by_conversation"]["c"]["seq"], 7);
            } else {
                assert_eq!(body["total"], 3);
                assert_eq!(body["latest_by_conversation"]["c"]["message_id"], "reply-2");
            }
        }
        db.close().await.unwrap();
        drop(store);
    }
}

#[tokio::test]
async fn returned_latest_still_rejects_bad_fields_while_count_only_summary_does_not_audit() {
    for mutation in [
        "UPDATE messages SET id='' WHERE id='reply-2'",
        "UPDATE messages SET id=NULL WHERE id='reply-2'",
        "UPDATE messages SET id=x'61' WHERE id='reply-2'",
        "UPDATE messages SET id=CAST(x'80' AS TEXT) WHERE id='reply-2'",
        "UPDATE messages SET seq=7.5 WHERE id='reply-2'",
        "UPDATE messages SET seq=9007199254740992 WHERE id='reply-2'",
        "UPDATE messages SET from_user_id='missing' WHERE id='reply-2'",
        "DELETE FROM messages WHERE id='root'",
    ] {
        let (dir, store) = store("workspace_member").await;
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(dir.path().join("message-unread.sqlite"))
                .foreign_keys(false),
        )
        .await
        .unwrap();
        db.execute(sqlx::raw_sql(mutation)).await.unwrap();
        for path in [
            "/api/workspaces/w/message-unread",
            "/api/message-unread-summary",
            "/api/channels/c/message-unread",
        ] {
            let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
            request
                .extensions_mut()
                .insert(UserId::parse("local").unwrap());
            let response = router(store.message_unread())
                .oneshot(request)
                .await
                .unwrap();
            let expected = if path == "/api/message-unread-summary" {
                axum::http::StatusCode::OK
            } else {
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            };
            assert_eq!(response.status(), expected, "{mutation} at {path}");
        }
        db.close().await.unwrap();
        drop(store);
    }
}

#[tokio::test]
async fn returned_pointer_identity_requires_utf8_and_preserves_nul() {
    let (dir, store) = store("workspace_member").await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(dir.path().join("message-unread.sqlite"))
            .foreign_keys(false),
    )
    .await
    .unwrap();
    // A TEXT storage class and nonempty SQL comparison do not prove Rust String
    // decodability. JSON validity also does not validate the UTF-8 bytes.
    let witness: (String, i64, i64) = sqlx::query_as(
        "SELECT typeof(CAST(x'80' AS TEXT)), \
         CAST(x'80' AS TEXT) != '' COLLATE BINARY, \
         json_valid(json_quote(CAST(x'80' AS TEXT)))",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(witness, ("text".into(), 1, 1));
    assert!(
        sqlx::query_scalar::<_, String>("SELECT CAST(x'80' AS TEXT)")
            .fetch_one(&mut db)
            .await
            .is_err()
    );
    for (expression, accepted) in [
        ("SELECT NULL", false),
        ("SELECT ''", false),
        ("SELECT x'61'", false),
        ("SELECT CAST(x'00' AS TEXT)", true),
        ("SELECT CAST(x'610062' AS TEXT)", true),
        ("SELECT '合法 opaque 身份'", true),
    ] {
        let raw = sqlx::query_scalar::<_, String>(expression)
            .fetch_one(&mut db)
            .await;
        let accepted_by_decoder = raw.ok().is_some_and(|id| MessageId::parse(id).is_ok());
        assert_eq!(accepted_by_decoder, accepted, "{expression}");
    }
    // Keep NUL in the latest candidate, so the actual three routes and pointer
    // serialization must accept it, not merely count a hidden middle row.
    sqlx::query("UPDATE messages SET id=?,seq=9007199254740991 WHERE id='reply-2'")
        .bind("a\0b")
        .execute(&mut db)
        .await
        .unwrap();
    for path in [
        "/api/workspaces/w/message-unread",
        "/api/message-unread-summary",
        "/api/channels/c/message-unread",
    ] {
        let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = router(store.message_unread())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK, "{path}");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        if path != "/api/message-unread-summary" {
            assert_eq!(body["latest_by_conversation"]["c"]["message_id"], "a\0b");
            assert_eq!(
                body["latest_by_conversation"]["c"]["seq"],
                9_007_199_254_740_991_i64
            );
        }
    }
    db.close().await.unwrap();
    drop(store);
}

#[tokio::test]
async fn aggregate_rollups_keep_sparse_ordinals_and_conversation_identity() {
    let (dir, store) = store("workspace_member").await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(dir.path().join("message-unread.sqlite")),
    )
    .await
    .unwrap();
    sqlx::query("WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<600) INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) SELECT 'packet-'||n,'c','user','other','packet',1000+n,1 FROM numbers")
        .execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES('gap-c','c','user','other','gap',1000000000,1),('gap-c2','c2','user','other','gap',1000000000,1)")
        .execute(&mut db).await.unwrap();
    for path in [
        "/api/workspaces/w/message-unread",
        "/api/message-unread-summary",
    ] {
        let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
        let response = router(store.message_unread())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        if path.ends_with("summary") {
            assert_eq!(body, serde_json::json!({"by_workspace":{"w":605}}));
        } else {
            assert_eq!(body["total"], 605);
            assert_eq!(body["by_conversation"], serde_json::json!({"c":603,"c2":2}));
            for conversation in ["c", "c2"] {
                assert_eq!(
                    body["latest_by_conversation"][conversation],
                    serde_json::json!({"conversation_id":conversation,"message_id":format!("gap-{conversation}"),"seq":1000000000,"thread_root_id":null})
                );
            }
        }
    }
    db.close().await.unwrap();
    drop(store);
}

#[tokio::test]
async fn publication_combines_workspace_counts_and_keeps_tier_rules() {
    use silverhand_backend_core::conversation_messaging::messaging::unread::{
        UnreadAudience, UnreadReadModel,
    };
    use silverhand_backend_sqlite::viewer_read_model::unread_read::SqliteUnreadReadModel;
    let (dir, store) = store("workspace_member").await;
    let path = dir.path().join("message-unread.sqlite");
    let read = SqliteUnreadReadModel::open(&path).await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let conversation = ConversationId::parse("c").unwrap();
    for (mutation, expected_workspace, expected_badge, expected_latest) in [
        ("SELECT 1", 3, 3, Some(7)),
        (
            "INSERT INTO channel_attention_preferences VALUES('local','c','mentions',1); INSERT INTO notifications(id,kind,user_id,workspace_id,conversation_id,message_id,payload,created_at) VALUES('n1','mention_self','local','w','c','root','{}',1),('n2','mention_self','local','w','c','root','{}',2)",
            3,
            2,
            Some(7),
        ),
        (
            "INSERT INTO workspace_attention_preferences VALUES('local','w',1,1)",
            3,
            1,
            Some(7),
        ),
        (
            "INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES('c3','w','workspace','fixed','Fixed','fixed','idle','private',3,3); INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('c3','local',3); INSERT INTO messages(id,conversation_id,from_kind,from_user_id,content,seq,created_at) VALUES('fixed-message','c3','user','other','fixed',1,3)",
            4,
            2,
            Some(7),
        ),
        (
            "UPDATE conversations SET archived_at=9 WHERE id='c'",
            2,
            1,
            None,
        ),
    ] {
        db.execute(sqlx::raw_sql(mutation)).await.unwrap();
        let Some(UnreadAudience::Workspace { viewers, .. }) =
            read.unread_audience(&conversation).await.unwrap()
        else {
            panic!("expected workspace audience")
        };
        assert_eq!(viewers.len(), 1);
        let viewer = &viewers[0];
        assert_eq!(viewer.viewer.as_str(), "local");
        assert_eq!(
            viewer.workspace_unread_count, expected_workspace,
            "{mutation}"
        );
        assert_eq!(viewer.badge_count, expected_badge, "{mutation}");
        assert_eq!(
            viewer.latest_unread.as_ref().map(|p| p.seq),
            expected_latest,
            "{mutation}"
        );
        assert_eq!(
            viewer.conversation_unread_count,
            if expected_latest.is_some() { 2 } else { 0 }
        );
    }
    db.close().await.unwrap();
    drop(read);
    drop(store);
}
