use silverhand_backend_core::{
    conversation_messaging::messaging::{
        CreatedMessage, FromKind, MessageContent, MessageId, MessageSeq, ThreadRootId,
        alert::{AlertPlace, AlertStanding, MessageAlertAudience},
    },
    foundation::identity::ConversationId,
};
use silverhand_backend_sqlite::viewer_read_model::unread_read::SqliteUnreadReadModel;
use sqlx::Connection;

use crate::current_schema;

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('loud','loud','Loud',1,1),('quiet','quiet','Quiet',1,2),\
         ('hushed','hushed','Hushed',1,3),('outsider','outsider','Outsider',1,4)",
        "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES ('w','W','w',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES \
         ('w','loud',1),('w','quiet',1),('w','hushed',1),('w','outsider',1)",
        seed_channel!(id = "c", workspace_id = "w"),
        "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES \
         ('c','loud',1),('c','quiet',1),('c','hushed',1)",
        "INSERT INTO channel_attention_preferences(user_id,conversation_id,mode,updated_at) \
         VALUES ('quiet','c','mentions',1)",
        "INSERT INTO workspace_attention_preferences(user_id,workspace_id,muted,updated_at) \
         VALUES ('hushed','w',1,1)",
        "INSERT INTO messages(id,conversation_id,from_kind,content,seq,created_at) VALUES \
         ('root','c','system','root',1,1)",
        "INSERT INTO thread_followers(thread_root_id,conversation_id,principal_kind,principal_id,\
         followed_at,source) VALUES ('root','c','human','quiet',1,'manual')",
        seed_direct_pair!(
            id = "pair",
            endpoint_a_kind = "human",
            endpoint_a_id = "loud",
            endpoint_b_kind = "human",
            endpoint_b_id = "outsider",
        ),
    ] {
        sqlx::query(sql).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

fn message(conversation: &str, thread: Option<&str>) -> CreatedMessage {
    CreatedMessage {
        client_message_id: None,
        id: MessageId::parse("m").unwrap(),
        conversation_id: ConversationId::parse(conversation).unwrap(),
        from_kind: FromKind::Agent,
        from_agent_id: None,
        from_user_id: None,
        author: None,
        content: MessageContent::parse("hi").unwrap(),
        attachments: Vec::new(),
        seq: MessageSeq::try_from(2).unwrap(),
        created_at_ms: 2,
        thread_root_id: thread.map(|id| ThreadRootId::parse(id).unwrap()),
        options: None,
    }
}

async fn standings(
    model: &SqliteUnreadReadModel,
    message: &CreatedMessage,
) -> Vec<(String, AlertStanding)> {
    model
        .alert_audience(&message.conversation_id, message)
        .await
        .unwrap()
        .unwrap()
        .recipients
        .into_iter()
        .map(|r| {
            assert_eq!(
                r.handle.as_str(),
                r.viewer.as_str(),
                "seeded handles equal ids"
            );
            (r.viewer.as_str().to_owned(), r.standing)
        })
        .collect()
}

fn channel(ambient: bool, follows_thread: bool) -> AlertStanding {
    AlertStanding::Channel {
        ambient,
        follows_thread,
    }
}

#[tokio::test]
async fn recipients_carry_effective_level_and_thread_following() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alert.sqlite");
    seed(&path).await;
    let model = SqliteUnreadReadModel::open(&path).await.unwrap();

    assert_eq!(
        standings(&model, &message("c", None)).await,
        vec![
            ("hushed".to_owned(), channel(false, false)),
            ("loud".to_owned(), channel(true, false)),
            ("quiet".to_owned(), channel(false, false)),
        ],
        "non-members are not recipients; mentions mode and workspace mute are not ambient"
    );
    assert_eq!(
        standings(&model, &message("c", Some("root"))).await[2],
        ("quiet".to_owned(), channel(false, true))
    );
    assert_eq!(
        standings(&model, &message("pair", None)).await,
        vec![
            ("loud".to_owned(), AlertStanding::Direct),
            ("outsider".to_owned(), AlertStanding::Direct),
        ],
        "both human endpoints of a direct pair are direct recipients"
    );
    let place = |conversation| {
        let message = message(conversation, None);
        let model = &model;
        async move {
            model
                .alert_audience(&message.conversation_id, &message)
                .await
                .unwrap()
                .map(|audience| audience.place)
        }
    };
    let channel_place = place("c").await.unwrap();
    assert_eq!(channel_place.workspace.unwrap().as_str(), "w");
    assert_eq!(channel_place.channel_title.as_deref(), Some("c"));
    assert_eq!(
        place("pair").await,
        Some(AlertPlace {
            workspace: None,
            channel_title: None
        })
    );
    assert_eq!(place("gone").await, None);
}
