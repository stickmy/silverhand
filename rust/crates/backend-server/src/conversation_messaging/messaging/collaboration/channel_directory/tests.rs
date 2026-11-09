use std::{convert::Infallible, sync::Arc};

use axum::{
    body::{Body, to_bytes},
    extract::Extension,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::channel_directory::{
        ActiveChannelDirectoryPort, ActiveWorkspaceChannelDirectory, ArchivedSince,
        ArchivedWorkspaceChannelDirectory, ArchivedWorkspaceChannelDirectoryPort, ChannelSortState,
        DirectoryWorkspaceKind, GlobalArchivedChannelDirectory, GlobalArchivedChannelDirectoryPort,
        GlobalArchivedDirectoryRefusal, WorkspaceDirectoryRefusal,
    },
    foundation::identity::{UserId, WorkspaceId},
};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
use sqlx::{Connection, Executor};
use tower::ServiceExt;

use super::router;

fn fixture() -> Vec<Value> {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/fixtures/channel-directory-wire.json"
    )))
    .unwrap()
}

fn expected_body(entry: &Value, fixture: &[Value]) -> Value {
    if let Some(body_case) = entry.get("body_case").and_then(Value::as_str) {
        return expected_body(
            fixture
                .iter()
                .find(|candidate| candidate["name"] == body_case)
                .unwrap(),
            fixture,
        );
    }
    entry["body"].clone()
}

async fn seeded() -> (tempfile::TempDir, MessagingStore) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("channel-directory-wire.sqlite");
    let mut db = crate::current_schema::create_current_schema(&path).await;
    db.execute(sqlx::raw_sql(
        concat!(
            "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES ('local','channel-directory-local','Local',1,1); ",
            "INSERT INTO hosts(id,name,kind,created_at) VALUES ('directory-host','Directory Host','local',1); ",
            seed_agent!(
                id = "directory-agent",
                host_id = "directory-host",
                created_by_user_id = "local",
            ),
            "; ",
            "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) VALUES ('workspace-1','Workspace','channel-directory-wire',1,1), ('workspace-2','Forbidden','channel-directory-forbidden',1,1); ",
            "INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at) VALUES ('workspace-home','directory-agent',NULL,'directory-agent',1,1); ",
            "INSERT INTO workspace_users(workspace_id,user_id,joined_at) VALUES ('workspace-1','local',1),('workspace-home','local',1); ",
            seed_channel!(
                id = "conversation-public",
                workspace_id = "workspace-1",
                title = "Public",
                title_key = "directory-public",
                visibility = "public",
            ),
            "; ",
            seed_channel!(
                id = "conversation-member",
                workspace_id = "workspace-1",
                title = "Member",
                title_key = "directory-member",
                created_at = 2,
                updated_at = 2,
            ),
            "; ",
            seed_channel!(
                id = "conversation-hidden",
                workspace_id = "workspace-1",
                title = "Hidden",
                title_key = "directory-hidden",
                created_at = 4,
                updated_at = 4,
            ),
            "; ",
            seed_channel!(
                id = "conversation-archived",
                workspace_id = "workspace-1",
                title = "Archived",
                title_key = "directory-archived",
                archived_at = 10,
                created_at = 3,
                updated_at = 10,
            ),
            "; ",
            seed_channel!(
                id = "home-active",
                workspace_id = "workspace-home",
                roster_policy = "fixed",
                title = "Onboarding",
                title_key = "directory-home-active",
                created_at = 5,
                updated_at = 5,
            ),
            "; ",
            seed_channel!(
                id = "home-archived",
                workspace_id = "workspace-home",
                roster_policy = "fixed",
                title = "Home Archived",
                title_key = "directory-home-archived",
                archived_at = 20,
                created_at = 6,
                updated_at = 20,
            ),
            "; ",
            "INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES ('conversation-member','local',1),('conversation-archived','local',1), ('home-active','local',1),('home-archived','local',1); ",
            "INSERT INTO user_list_sort_mode(user_id,list_key,scope,mode,updated_at) VALUES ('local','channels:workspace-1','','manual',1); ",
            "INSERT INTO user_sort_entry(user_id,list_key,entity_id,rank,last_accessed_at,starred_at) VALUES ('local','channels:workspace-1','conversation-member',1,4,5), ('local','channels:workspace-1','conversation-public',2,3,NULL), ('local','channels:workspace-1','conversation-hidden',0,9,NULL); ",
        ),
    ))
    .await
    .unwrap();
    db.close().await.unwrap();
    let store = MessagingStore::open(path).await.unwrap();
    (directory, store)
}

async fn call(app: axum::Router, path: &str, authenticated: bool) -> (StatusCode, Value) {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    if authenticated {
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, body)
}

#[tokio::test]
async fn production_router_consumes_every_ts_signed_cell() {
    let (_directory, store) = seeded().await;
    let fixture = fixture();
    let app = router(store.channel_directory()).layer(Extension(UserId::parse("local").unwrap()));
    for entry in &fixture {
        let (status, body) = call(app.clone(), entry["path"].as_str().unwrap(), false).await;
        assert_eq!(
            status.as_u16(),
            entry["status"].as_u64().unwrap() as u16,
            "{}",
            entry["name"]
        );
        assert_eq!(body, expected_body(entry, &fixture), "{}", entry["name"]);
    }
}

#[derive(Clone)]
struct Spliced {
    viewer: UserId,
    workspace: WorkspaceId,
    global_filter: Option<WorkspaceId>,
}

fn other() -> UserId {
    UserId::parse("other").unwrap()
}
fn other_workspace() -> WorkspaceId {
    WorkspaceId::parse("other-workspace").unwrap()
}

impl ActiveChannelDirectoryPort for Spliced {
    type Error = Infallible;
    async fn active(
        &self,
        _: &UserId,
        _: &WorkspaceId,
    ) -> Result<Result<ActiveWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>, Self::Error>
    {
        Ok(Ok(ActiveWorkspaceChannelDirectory::projected(
            self.viewer.clone(),
            self.workspace.clone(),
            DirectoryWorkspaceKind::Regular,
            vec![],
            ChannelSortState::projected(self.viewer.clone(), self.workspace.clone(), None, vec![])
                .unwrap(),
        )
        .unwrap()))
    }
}

impl ArchivedWorkspaceChannelDirectoryPort for Spliced {
    type Error = Infallible;
    async fn archived(
        &self,
        _: &UserId,
        _: &WorkspaceId,
    ) -> Result<Result<ArchivedWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>, Self::Error>
    {
        Ok(Ok(ArchivedWorkspaceChannelDirectory::projected(
            self.viewer.clone(),
            self.workspace.clone(),
            DirectoryWorkspaceKind::Regular,
            vec![],
        )
        .unwrap()))
    }
}

impl GlobalArchivedChannelDirectoryPort for Spliced {
    type Error = Infallible;
    async fn archived_global(
        &self,
        _: &UserId,
        _: Option<&WorkspaceId>,
        _: Option<ArchivedSince>,
    ) -> Result<Result<GlobalArchivedChannelDirectory, GlobalArchivedDirectoryRefusal>, Self::Error>
    {
        Ok(Ok(GlobalArchivedChannelDirectory::projected(
            self.viewer.clone(),
            self.global_filter.clone(),
            vec![],
        )
        .unwrap()))
    }
}

#[tokio::test]
async fn success_fact_viewer_and_workspace_axes_bind_independently() {
    let local = UserId::parse("local").unwrap();
    let workspace = WorkspaceId::parse("workspace-1").unwrap();

    for (port, path) in [
        (
            Spliced {
                viewer: other(),
                workspace: workspace.clone(),
                global_filter: None,
            },
            "/api/workspaces/workspace-1/channels",
        ),
        (
            Spliced {
                viewer: local.clone(),
                workspace: other_workspace(),
                global_filter: None,
            },
            "/api/workspaces/workspace-1/channels",
        ),
        (
            Spliced {
                viewer: other(),
                workspace: workspace.clone(),
                global_filter: None,
            },
            "/api/workspaces/workspace-1/channels/archived",
        ),
        (
            Spliced {
                viewer: local.clone(),
                workspace: other_workspace(),
                global_filter: None,
            },
            "/api/workspaces/workspace-1/channels/archived",
        ),
        (
            Spliced {
                viewer: other(),
                workspace: workspace.clone(),
                global_filter: None,
            },
            "/api/archived-channels",
        ),
        (
            Spliced {
                viewer: local.clone(),
                workspace: workspace.clone(),
                global_filter: Some(other_workspace()),
            },
            "/api/archived-channels?workspace_id=workspace-1",
        ),
    ] {
        let app = router(port).layer(Extension(local.clone()));
        let (status, body) = call(app, path, false).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{path}");
        assert_eq!(body, json!({"error":"internal error"}), "{path}");
    }

    for (port, path) in [
        (
            Spliced {
                viewer: local.clone(),
                workspace: workspace.clone(),
                global_filter: None,
            },
            "/api/workspaces/workspace-1/channels",
        ),
        (
            Spliced {
                viewer: local.clone(),
                workspace: workspace.clone(),
                global_filter: None,
            },
            "/api/workspaces/workspace-1/channels/archived",
        ),
        (
            Spliced {
                viewer: local.clone(),
                workspace: workspace.clone(),
                global_filter: Some(workspace.clone()),
            },
            "/api/archived-channels?workspace_id=workspace-1",
        ),
    ] {
        let app = router(port).layer(Extension(local.clone()));
        let (status, _) = call(app, path, false).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
}

#[derive(Clone)]
struct Never(Arc<std::sync::atomic::AtomicUsize>);

macro_rules! never_read {
    ($trait_name:ident, $method:ident, $output:ty, $($arg:ident : $ty:ty),*) => {
        impl $trait_name for Never {
            type Error = Infallible;
            async fn $method(&self, $(_: $ty),*) -> Result<$output, Self::Error> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                unreachable!()
            }
        }
    };
}

never_read!(ActiveChannelDirectoryPort, active, Result<ActiveWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>, _viewer: &UserId, _workspace: &WorkspaceId);
never_read!(ArchivedWorkspaceChannelDirectoryPort, archived, Result<ArchivedWorkspaceChannelDirectory, WorkspaceDirectoryRefusal>, _viewer: &UserId, _workspace: &WorkspaceId);
never_read!(GlobalArchivedChannelDirectoryPort, archived_global, Result<GlobalArchivedChannelDirectory, GlobalArchivedDirectoryRefusal>, _viewer: &UserId, _workspace: Option<&WorkspaceId>, _since: Option<ArchivedSince>);

#[tokio::test]
async fn authentication_precedes_all_product_reads() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = router(Never(calls.clone()));
    for path in [
        "/api/workspaces/workspace-1/channels",
        "/api/workspaces/workspace-1/channels/archived",
        "/api/archived-channels",
    ] {
        assert_eq!(
            call(app.clone(), path, false).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}
