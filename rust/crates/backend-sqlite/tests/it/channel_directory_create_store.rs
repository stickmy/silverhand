use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        SendClock, SendEntropy,
        collaboration::channel_directory::{
            ChannelVisibility, HumanChannelCreatePort, HumanChannelCreateRefusal,
            HumanChannelCreateRequest,
        },
        conversation_title::CanonicalConversationTitle,
        environment::EnvironmentUnavailable,
    },
    foundation::identity::{UserId, WorkspaceId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore, collaboration::channel_directory::ChannelDirectoryCreateStoreError,
};
use sqlx::{Connection, Executor, Row, SqliteConnection};

use crate::current_schema;

#[derive(Clone)]
struct FixedEnvironment {
    clock_calls: Arc<AtomicUsize>,
    entropy_calls: Arc<AtomicUsize>,
    byte: u8,
}

impl FixedEnvironment {
    fn new(byte: u8) -> Self {
        Self {
            clock_calls: Arc::new(AtomicUsize::new(0)),
            entropy_calls: Arc::new(AtomicUsize::new(0)),
            byte,
        }
    }
}

impl SendClock for FixedEnvironment {
    fn now_unix_ms(&self) -> Result<i64, EnvironmentUnavailable> {
        self.clock_calls.fetch_add(1, Ordering::SeqCst);
        Ok(77)
    }
}

impl SendEntropy for FixedEnvironment {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
        self.entropy_calls.fetch_add(1, Ordering::SeqCst);
        bytes.fill(self.byte);
        Ok(())
    }
}

async fn seed(path: &std::path::Path) {
    let mut db = current_schema::create_current_schema(path).await;
    for sql in [
        "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES \
         ('u','create-viewer','Viewer',1,1),('v','create-member','Member',1,1),\
         ('x','create-outsider','Outsider',1,1)",
        "INSERT INTO hosts(id,name,kind,created_at) VALUES('h','Create Host','local',1)",
        "INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) VALUES \
         ('a','create-agent-a','claude','h',1,1,'u'),\
         ('b','create-agent-b','claude','h',1,1,'u')",
        "INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at) VALUES \
         ('w','Create Workspace','create-workspace',NULL,1,1),\
         ('forbidden','Forbidden Workspace','forbidden-workspace',NULL,1,1),\
         ('home','Create Agent Home',NULL,'a',1,1)",
        "INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES \
         ('w','u',1,'admin'),('w','v',1,'member'),('home','u',1,'admin')",
        "INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('w','a',1)",
        "INSERT INTO conversations(\
           id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at\
         ) VALUES('taken','w','workspace','open','Taken','taken','idle','private',1,1)",
    ] {
        db.execute(sql).await.unwrap();
    }
    db.close().await.unwrap();
}

fn request(
    title: &str,
    agents: &[&str],
    users: Option<&[&str]>,
    visibility: ChannelVisibility,
) -> HumanChannelCreateRequest {
    HumanChannelCreateRequest::new(
        CanonicalConversationTitle::parse(title).unwrap(),
        agents.iter().map(|value| (*value).to_owned()).collect(),
        users.map(|values| values.iter().map(|value| (*value).to_owned()).collect()),
        Some(visibility),
    )
}

fn viewer() -> UserId {
    UserId::parse("u").unwrap()
}

fn workspace(raw: &str) -> WorkspaceId {
    WorkspaceId::parse(raw).unwrap()
}

#[tokio::test]
async fn regular_and_agent_home_create_exact_founding_rosters() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-create-success.sqlite");
    seed(&path).await;

    let regular = MessagingStore::open_with(&path, FixedEnvironment::new(1))
        .await
        .unwrap()
        .channel_directory_create();
    let committed = regular
        .create_human_channel(
            viewer(),
            workspace("w"),
            request(
                "  General   Discussion  ",
                &["a", "a"],
                Some(&["v", "u", "v"]),
                ChannelVisibility::Public,
            ),
        )
        .await
        .unwrap()
        .unwrap()
        .bind(
            &viewer(),
            &workspace("w"),
            &CanonicalConversationTitle::parse("General   Discussion").unwrap(),
            ChannelVisibility::Public,
        )
        .unwrap();
    assert_eq!(committed.channel().title(), "General   Discussion");
    assert_eq!(committed.address().key(), "general   discussion");

    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    let id = committed.channel().conversation().as_str();
    let row = sqlx::query(
        "SELECT roster_policy,visibility,status,title,title_key,created_at,updated_at \
         FROM conversations WHERE id=?",
    )
    .bind(id)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("roster_policy"), "open");
    assert_eq!(row.get::<String, _>("visibility"), "public");
    assert_eq!(row.get::<String, _>("status"), "idle");
    assert_eq!(row.get::<String, _>("title"), "General   Discussion");
    assert_eq!(row.get::<String, _>("title_key"), "general   discussion");
    assert_eq!(row.get::<i64, _>("created_at"), 77);
    assert_eq!(row.get::<i64, _>("updated_at"), 77);
    let agents = sqlx::query_scalar::<_, String>(
        "SELECT agent_id FROM conversation_agents WHERE conversation_id=? ORDER BY agent_id",
    )
    .bind(id)
    .fetch_all(&mut db)
    .await
    .unwrap();
    let users = sqlx::query_scalar::<_, String>(
        "SELECT user_id FROM conversation_users WHERE conversation_id=? ORDER BY user_id",
    )
    .bind(id)
    .fetch_all(&mut db)
    .await
    .unwrap();
    assert_eq!(agents, ["a"]);
    assert_eq!(users, ["u", "v"]);
    db.close().await.unwrap();

    let home = MessagingStore::open_with(&path, FixedEnvironment::new(2))
        .await
        .unwrap()
        .channel_directory_create();
    let home = home
        .create_human_channel(
            viewer(),
            workspace("home"),
            request("Private DM", &[], None, ChannelVisibility::Private),
        )
        .await
        .unwrap()
        .unwrap()
        .bind(
            &viewer(),
            &workspace("home"),
            &CanonicalConversationTitle::parse("Private DM").unwrap(),
            ChannelVisibility::Private,
        )
        .unwrap();
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    let id = home.channel().conversation().as_str();
    let row = sqlx::query("SELECT roster_policy,visibility FROM conversations WHERE id=?")
        .bind(id)
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("roster_policy"), "fixed");
    assert_eq!(row.get::<String, _>("visibility"), "private");
    let agents = sqlx::query_scalar::<_, String>(
        "SELECT agent_id FROM conversation_agents WHERE conversation_id=?",
    )
    .bind(id)
    .fetch_all(&mut db)
    .await
    .unwrap();
    let users = sqlx::query_scalar::<_, String>(
        "SELECT user_id FROM conversation_users WHERE conversation_id=?",
    )
    .bind(id)
    .fetch_all(&mut db)
    .await
    .unwrap();
    assert_eq!(agents, ["a"]);
    assert_eq!(users, ["u"]);
}

#[tokio::test]
async fn preflight_refusals_preserve_route_order_and_do_not_consume_host_or_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-create-refusals.sqlite");
    seed(&path).await;
    let environment = FixedEnvironment::new(3);
    let create = MessagingStore::open_with(&path, environment.clone())
        .await
        .unwrap()
        .channel_directory_create();
    let mut writer = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    writer.execute("BEGIN IMMEDIATE").await.unwrap();

    let cases = [
        (
            workspace("missing"),
            request("A", &["missing"], None, ChannelVisibility::Public),
            HumanChannelCreateRefusal::WorkspaceNotFound,
        ),
        (
            workspace("forbidden"),
            request("B", &[], None, ChannelVisibility::Private),
            HumanChannelCreateRefusal::Forbidden,
        ),
        (
            workspace("home"),
            request("C", &["missing"], None, ChannelVisibility::Public),
            HumanChannelCreateRefusal::AgentHomePublic,
        ),
        (
            workspace("home"),
            request("D", &["a"], None, ChannelVisibility::Private),
            HumanChannelCreateRefusal::AgentHomeExplicitMembers,
        ),
        (
            workspace("w"),
            request(
                "E",
                &["missing", "b"],
                Some(&["x"]),
                ChannelVisibility::Private,
            ),
            HumanChannelCreateRefusal::AgentNotFound("missing".into()),
        ),
        (
            workspace("w"),
            request("F", &["b"], Some(&["x"]), ChannelVisibility::Private),
            HumanChannelCreateRefusal::AgentNotWorkspaceMember("b".into()),
        ),
        (
            workspace("w"),
            request("G", &[], Some(&["x"]), ChannelVisibility::Private),
            HumanChannelCreateRefusal::UserNotWorkspaceMember("x".into()),
        ),
        (
            workspace("w"),
            request("Taken", &[], None, ChannelVisibility::Private),
            HumanChannelCreateRefusal::TitleTaken,
        ),
    ];
    for (workspace, request, expected) in cases {
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            create.create_human_channel(viewer(), workspace, request),
        )
        .await
        .expect("preflight refusal must not wait for the held writer")
        .unwrap();
        assert_eq!(result, Err(expected));
    }
    assert_eq!(environment.clock_calls.load(Ordering::SeqCst), 0);
    assert_eq!(environment.entropy_calls.load(Ordering::SeqCst), 0);
    writer.execute("ROLLBACK").await.unwrap();
}

#[tokio::test]
async fn successful_path_waits_for_writer_then_revalidates_each_changed_authority() {
    async fn wait_for_preflight(environment: &FixedEnvironment, attempt: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while environment.entropy_calls.load(Ordering::SeqCst) < attempt {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the request must complete preflight before authority changes");
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-create-authority-race.sqlite");
    seed(&path).await;
    let environment = FixedEnvironment::new(4);
    let create = MessagingStore::open_with(&path, environment.clone())
        .await
        .unwrap()
        .channel_directory_create();
    let mut writer = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    writer.execute("BEGIN IMMEDIATE").await.unwrap();

    let governance_create = create.clone();
    let pending = tokio::spawn(async move {
        governance_create
            .create_human_channel(
                viewer(),
                workspace("w"),
                request("Writer Race", &["a"], None, ChannelVisibility::Private),
            )
            .await
    });
    wait_for_preflight(&environment, 1).await;
    assert!(
        !pending.is_finished(),
        "admitted write must wait for reservation"
    );
    writer
        .execute("UPDATE workspace_users SET role='member' WHERE workspace_id='w' AND user_id='u'")
        .await
        .unwrap();
    writer.execute("COMMIT").await.unwrap();

    let result = pending.await.unwrap().unwrap();
    assert_eq!(result, Err(HumanChannelCreateRefusal::Forbidden));
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE title='Writer Race'")
            .fetch_one(&mut writer)
            .await
            .unwrap();
    assert_eq!(count, 0);

    writer
        .execute("UPDATE workspace_users SET role='admin' WHERE workspace_id='w' AND user_id='u'")
        .await
        .unwrap();
    writer.execute("BEGIN IMMEDIATE").await.unwrap();
    let membership_create = create.clone();
    let pending = tokio::spawn(async move {
        membership_create
            .create_human_channel(
                viewer(),
                workspace("w"),
                request("Membership Race", &["a"], None, ChannelVisibility::Private),
            )
            .await
    });
    wait_for_preflight(&environment, 2).await;
    assert!(!pending.is_finished());
    writer
        .execute("DELETE FROM workspace_members WHERE workspace_id='w' AND agent_id='a'")
        .await
        .unwrap();
    writer.execute("COMMIT").await.unwrap();
    assert_eq!(
        pending.await.unwrap().unwrap(),
        Err(HumanChannelCreateRefusal::AgentNotWorkspaceMember(
            "a".into()
        ))
    );

    writer
        .execute("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('w','a',2)")
        .await
        .unwrap();
    writer.execute("BEGIN IMMEDIATE").await.unwrap();
    let title_create = create.clone();
    let pending = tokio::spawn(async move {
        title_create
            .create_human_channel(
                viewer(),
                workspace("w"),
                request("Title Race", &[], None, ChannelVisibility::Private),
            )
            .await
    });
    wait_for_preflight(&environment, 3).await;
    assert!(!pending.is_finished());
    writer
        .execute(
            "INSERT INTO conversations(\
               id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at\
             ) VALUES('racing-title','w','workspace','open','Title Race','title race','idle','private',2,2)",
        )
        .await
        .unwrap();
    writer.execute("COMMIT").await.unwrap();
    assert_eq!(
        pending.await.unwrap().unwrap(),
        Err(HumanChannelCreateRefusal::TitleTaken)
    );
}

#[tokio::test]
async fn post_insert_corruption_rolls_back_and_repair_allows_same_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-create-corruption.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    db.execute(
        "CREATE TRIGGER corrupt_new_channel AFTER INSERT ON conversations \
         WHEN NEW.title='Corruption Probe' BEGIN \
           UPDATE conversations SET title=' Corruption Probe ' WHERE id=NEW.id; \
         END",
    )
    .await
    .unwrap();
    db.close().await.unwrap();

    let create = MessagingStore::open_with(&path, FixedEnvironment::new(5))
        .await
        .unwrap()
        .channel_directory_create();
    let error = create
        .create_human_channel(
            viewer(),
            workspace("w"),
            request("Corruption Probe", &["a"], None, ChannelVisibility::Private),
        )
        .await
        .unwrap_err();
    let source = error.source().unwrap();
    let source = source
        .downcast_ref::<ChannelDirectoryCreateStoreError>()
        .unwrap();
    assert!(matches!(
        source,
        ChannelDirectoryCreateStoreError::Corrupt {
            field: "conversations.title:not_canonical"
        }
    ));

    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    let counts: (i64, i64, i64) =
        sqlx::query_as("SELECT (SELECT COUNT(*) FROM conversations), (SELECT COUNT(*) FROM conversation_agents), (SELECT COUNT(*) FROM conversation_users)")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(
        counts,
        (1, 0, 0),
        "strict decode failure must roll back row and roster"
    );
    db.execute("DROP TRIGGER corrupt_new_channel")
        .await
        .unwrap();
    db.execute(
        "CREATE TRIGGER corrupt_new_channel AFTER INSERT ON conversations \
         WHEN NEW.title='Corruption Probe' BEGIN \
           UPDATE conversations SET title_key='wrong' WHERE id=NEW.id; \
         END",
    )
    .await
    .unwrap();
    db.close().await.unwrap();

    let error = create
        .create_human_channel(
            viewer(),
            workspace("w"),
            request("Corruption Probe", &["a"], None, ChannelVisibility::Private),
        )
        .await
        .unwrap_err();
    let source = error.source().unwrap();
    let source = source
        .downcast_ref::<ChannelDirectoryCreateStoreError>()
        .unwrap();
    assert!(matches!(
        source,
        ChannelDirectoryCreateStoreError::Corrupt {
            field: "conversations.title_key"
        }
    ));

    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    let counts: (i64, i64, i64) =
        sqlx::query_as("SELECT (SELECT COUNT(*) FROM conversations), (SELECT COUNT(*) FROM conversation_agents), (SELECT COUNT(*) FROM conversation_users)")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(
        counts,
        (1, 0, 0),
        "strict decode failure must roll back row and roster"
    );
    db.execute("DROP TRIGGER corrupt_new_channel")
        .await
        .unwrap();
    db.close().await.unwrap();

    let repaired = create
        .create_human_channel(
            viewer(),
            workspace("w"),
            request("Corruption Probe", &["a"], None, ChannelVisibility::Private),
        )
        .await
        .unwrap();
    assert!(repaired.is_ok());
}

#[tokio::test]
async fn corrupt_governance_token_is_not_downgraded_to_a_product_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("channel-create-authority-corruption.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    db.execute("UPDATE workspace_users SET role='root' WHERE workspace_id='w' AND user_id='u'")
        .await
        .unwrap();
    db.close().await.unwrap();

    let environment = FixedEnvironment::new(6);
    let create = MessagingStore::open_with(&path, environment.clone())
        .await
        .unwrap()
        .channel_directory_create();
    let error = create
        .create_human_channel(
            viewer(),
            workspace("w"),
            request("Bad Role", &[], None, ChannelVisibility::Private),
        )
        .await
        .unwrap_err();
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<ChannelDirectoryCreateStoreError>()
        .unwrap();
    assert!(matches!(
        source,
        ChannelDirectoryCreateStoreError::Corrupt {
            field: "workspace_users.role"
        }
    ));
    assert_eq!(environment.clock_calls.load(Ordering::SeqCst), 0);
    assert_eq!(environment.entropy_calls.load(Ordering::SeqCst), 0);

    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute("PRAGMA ignore_check_constraints=ON")
        .await
        .unwrap();
    db.execute(
        "UPDATE workspace_users SET role='owner' WHERE workspace_id='w' AND user_id='u'; \
         UPDATE workspaces SET name_key=NULL WHERE id='w'",
    )
    .await
    .unwrap();
    db.close().await.unwrap();
    let error = create
        .create_human_channel(
            viewer(),
            workspace("w"),
            request("Bad Role", &[], None, ChannelVisibility::Private),
        )
        .await
        .unwrap_err();
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<ChannelDirectoryCreateStoreError>()
        .unwrap();
    assert!(matches!(
        source,
        ChannelDirectoryCreateStoreError::Corrupt {
            field: "workspaces.owner_agent_id:name_key"
        }
    ));
    assert_eq!(environment.clock_calls.load(Ordering::SeqCst), 0);
    assert_eq!(environment.entropy_calls.load(Ordering::SeqCst), 0);

    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute("UPDATE workspaces SET name_key='create-workspace' WHERE id='w'")
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(
        create
            .create_human_channel(
                viewer(),
                workspace("w"),
                request("Bad Role", &[], None, ChannelVisibility::Private),
            )
            .await
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn agent_founding_row_timestamp_corruption_rolls_back_the_complete_create() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("channel-create-agent-roster-corruption.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute(
        "CREATE TRIGGER corrupt_agent_joined_at AFTER INSERT ON conversation_agents BEGIN \
           UPDATE conversation_agents SET joined_at=-1 \
           WHERE conversation_id=NEW.conversation_id AND agent_id=NEW.agent_id; \
         END",
    )
    .await
    .unwrap();
    db.close().await.unwrap();

    let create = MessagingStore::open_with(&path, FixedEnvironment::new(7))
        .await
        .unwrap()
        .channel_directory_create();
    let error = create
        .create_human_channel(
            viewer(),
            workspace("w"),
            request(
                "Agent Roster Probe",
                &["a"],
                None,
                ChannelVisibility::Private,
            ),
        )
        .await
        .unwrap_err();
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<ChannelDirectoryCreateStoreError>()
        .unwrap();
    assert!(matches!(
        source,
        ChannelDirectoryCreateStoreError::Corrupt {
            field: "conversation_agents.joined_at"
        }
    ));

    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM conversations WHERE title='Agent Roster Probe'",
        )
        .fetch_one(&mut db)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_agents WHERE joined_at=-1")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        0
    );
    db.execute("DROP TRIGGER corrupt_agent_joined_at")
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(
        create
            .create_human_channel(
                viewer(),
                workspace("w"),
                request(
                    "Agent Roster Probe",
                    &["a"],
                    None,
                    ChannelVisibility::Private
                ),
            )
            .await
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn user_founding_row_timestamp_corruption_rolls_back_the_complete_create() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("channel-create-user-roster-corruption.sqlite");
    seed(&path).await;
    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    db.execute(
        "CREATE TRIGGER corrupt_user_joined_at AFTER INSERT ON conversation_users BEGIN \
           UPDATE conversation_users SET joined_at=-2 \
           WHERE conversation_id=NEW.conversation_id AND user_id=NEW.user_id; \
         END",
    )
    .await
    .unwrap();
    db.close().await.unwrap();

    let create = MessagingStore::open_with(&path, FixedEnvironment::new(8))
        .await
        .unwrap()
        .channel_directory_create();
    let error = create
        .create_human_channel(
            viewer(),
            workspace("w"),
            request(
                "User Roster Probe",
                &[],
                Some(&["v"]),
                ChannelVisibility::Private,
            ),
        )
        .await
        .unwrap_err();
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<ChannelDirectoryCreateStoreError>()
        .unwrap();
    assert!(matches!(
        source,
        ChannelDirectoryCreateStoreError::Corrupt {
            field: "conversation_users.joined_at"
        }
    ));

    let mut db = SqliteConnection::connect(path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM conversations WHERE title='User Roster Probe'",
        )
        .fetch_one(&mut db)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_users WHERE joined_at=-2")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        0
    );
    db.execute("DROP TRIGGER corrupt_user_joined_at")
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(
        create
            .create_human_channel(
                viewer(),
                workspace("w"),
                request(
                    "User Roster Probe",
                    &[],
                    Some(&["v"]),
                    ChannelVisibility::Private,
                ),
            )
            .await
            .unwrap()
            .is_ok()
    );
}
