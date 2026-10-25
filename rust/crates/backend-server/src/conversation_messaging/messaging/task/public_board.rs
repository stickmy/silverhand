//! Bounded human task-board HTTP adapters.

use std::{collections::HashMap, sync::Arc};

use axum::{
    Json, Router,
    extract::{Extension, RawQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use silverhand_backend_core::conversation_messaging::messaging::task::{
    HumanTaskBoardRead, TaskBoardAssigneeFilter, TaskBoardColumnRequest, TaskBoardContainer,
    TaskBoardCreatorFilter, TaskBoardCursor, TaskBoardFilters, TaskBoardPerson, TaskId, TaskNumber,
    TaskStatus, TaskSummarySource,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};

use super::wire::project_summary;
use crate::foundation::http_error::plain_500 as safe_500;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;

pub struct HumanTaskBoardState<R> {
    pub read: R,
}

pub fn human_task_board_router<R>(state: HumanTaskBoardState<R>) -> Router
where
    R: HumanTaskBoardRead + Send + Sync + 'static,
{
    Router::new()
        .route("/api/tasks/board/column", get(column::<R>))
        .route("/api/tasks/board/counts", get(counts::<R>))
        .route("/api/tasks/board/facets", get(facets::<R>))
        .with_state(Arc::new(state))
}

async fn column<R: HumanTaskBoardRead + Send + Sync + 'static>(
    State(state): State<Arc<HumanTaskBoardState<R>>>,
    principal: Option<Extension<UserId>>,
    RawQuery(raw): RawQuery,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        return safe_500();
    };
    let values = query_map(
        raw.as_deref(),
        &[
            "workspace_id",
            "channel_id",
            "container",
            "assignee_type",
            "assignee_id",
            "creator_type",
            "creator_id",
            "status",
            "cursor",
            "limit",
        ],
    );
    let Some(status) = values.get("status").and_then(|s| TaskStatus::parse(s).ok()) else {
        return invalid_query();
    };
    let filters = match filters(&values, viewer) {
        Ok(value) => value,
        Err(()) => return invalid_query(),
    };
    let limit = match values.get("limit") {
        None => DEFAULT_LIMIT,
        Some(raw) => match super::wire_number::parse_canonical_limit(raw, MAX_LIMIT) {
            Some(value) => value,
            _ => return invalid_query(),
        },
    };
    let cursor = match values.get("cursor") {
        None => None,
        Some(raw) => match decode_cursor(raw) {
            Some(value) => Some(value),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"invalid cursor","code":"invalid_cursor"})),
                )
                    .into_response();
            }
        },
    };
    let page = match state
        .read
        .column(&TaskBoardColumnRequest {
            filters,
            status,
            cursor,
            limit,
        })
        .await
    {
        Ok(page) => page,
        Err(_) => return safe_500(),
    };
    let next_cursor = if page.has_more {
        page.rows
            .last()
            .map(|row| encode_cursor(row.task().task_number(), row.task().id()))
    } else {
        None
    };
    let mut items = Vec::with_capacity(page.rows.len());
    for row in &page.rows {
        match project_summary(TaskSummarySource {
            task: row.task(),
            display: row.display(),
        }) {
            Ok(item) => items.push(item),
            Err(_) => return safe_500(),
        }
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": status.as_str(), "items": items, "next_cursor": next_cursor,
        })),
    )
        .into_response()
}

async fn counts<R: HumanTaskBoardRead + Send + Sync + 'static>(
    State(state): State<Arc<HumanTaskBoardState<R>>>,
    principal: Option<Extension<UserId>>,
    RawQuery(raw): RawQuery,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        return safe_500();
    };
    let values = query_map(
        raw.as_deref(),
        &[
            "workspace_id",
            "channel_id",
            "container",
            "assignee_type",
            "assignee_id",
            "creator_type",
            "creator_id",
        ],
    );
    let filters = match filters(&values, viewer) {
        Ok(value) => value,
        Err(()) => return invalid_query(),
    };
    let counts = match state.read.counts(&filters).await {
        Ok(value) => value,
        Err(_) => return safe_500(),
    };
    (StatusCode::OK, Json(serde_json::json!({"counts": {
        "todo": counts.todo.get(), "in_progress": counts.in_progress.get(),
        "in_review": counts.in_review.get(), "done": counts.done.get(), "closed": counts.closed.get(),
    }}))).into_response()
}

async fn facets<R: HumanTaskBoardRead + Send + Sync + 'static>(
    State(state): State<Arc<HumanTaskBoardState<R>>>,
    principal: Option<Extension<UserId>>,
    RawQuery(raw): RawQuery,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        return safe_500();
    };
    let values = query_map(raw.as_deref(), &["container"]);
    let container = match values.get("container") {
        None => None,
        Some(raw) => match parse_container(raw) {
            Some(value) => Some(value),
            None => return invalid_query(),
        },
    };
    let value = match state.read.facets(&viewer, container).await {
        Ok(value) => value,
        Err(_) => return safe_500(),
    };
    let people = |items: &[TaskBoardPerson]| {
        items
            .iter()
            .map(|person| match person {
                TaskBoardPerson::Agent { id, name } => {
                    serde_json::json!({"type":"agent","id":id.as_str(),"name":name})
                }
                TaskBoardPerson::User { id, name } => {
                    serde_json::json!({"type":"user","id":id.as_str(),"name":name})
                }
            })
            .collect::<Vec<_>>()
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "assignees": people(&value.assignees), "creators": people(&value.creators),
            "has_unassigned": value.has_unassigned,
        })),
    )
        .into_response()
}

fn query_map(raw: Option<&str>, allowed: &[&str]) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for (key, value) in form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        if !allowed.contains(&key.as_ref()) {
            continue;
        }
        if value.is_empty() && key.as_ref() != "status" {
            continue;
        }
        values
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    values
}

fn filters(values: &HashMap<String, String>, viewer: UserId) -> Result<TaskBoardFilters, ()> {
    let workspace = values
        .get("workspace_id")
        .map(WorkspaceId::parse)
        .transpose()
        .map_err(|_| ())?;
    let conversation = values
        .get("channel_id")
        .map(ConversationId::parse)
        .transpose()
        .map_err(|_| ())?;
    let container = values
        .get("container")
        .map(|v| parse_container(v).ok_or(()))
        .transpose()?;
    let assignee = match (
        values.get("assignee_type").map(String::as_str),
        values.get("assignee_id"),
    ) {
        (None, None) => None,
        (Some("unassigned"), None) => Some(TaskBoardAssigneeFilter::Unassigned),
        (Some("agent"), Some(id)) => Some(TaskBoardAssigneeFilter::Agent(
            AgentId::parse(id).map_err(|_| ())?,
        )),
        (Some("user"), Some(id)) => Some(TaskBoardAssigneeFilter::User(
            UserId::parse(id).map_err(|_| ())?,
        )),
        _ => return Err(()),
    };
    let creator = match (
        values.get("creator_type").map(String::as_str),
        values.get("creator_id"),
    ) {
        (None, None) => None,
        (Some("agent"), Some(id)) => Some(TaskBoardCreatorFilter::Agent(
            AgentId::parse(id).map_err(|_| ())?,
        )),
        (Some("user"), Some(id)) => Some(TaskBoardCreatorFilter::User(
            UserId::parse(id).map_err(|_| ())?,
        )),
        _ => return Err(()),
    };
    Ok(TaskBoardFilters {
        viewer,
        workspace,
        conversation,
        container,
        assignee,
        creator,
    })
}

fn parse_container(raw: &str) -> Option<TaskBoardContainer> {
    match raw {
        "workspace" => Some(TaskBoardContainer::Workspace),
        "agent_space" => Some(TaskBoardContainer::AgentSpace),
        "direct_messages" => Some(TaskBoardContainer::DirectMessages),
        _ => None,
    }
}

fn decode_cursor(raw: &str) -> Option<TaskBoardCursor> {
    let bytes = URL_SAFE_NO_PAD.decode(raw).ok()?;
    if URL_SAFE_NO_PAD.encode(&bytes) != raw {
        return None;
    }
    let (number, id): (i64, String) = serde_json::from_slice(&bytes).ok()?;
    Some(TaskBoardCursor {
        task_number: TaskNumber::new(number).ok()?,
        task_id: TaskId::parse(id).ok()?,
    })
}

fn encode_cursor(number: TaskNumber, id: &TaskId) -> String {
    URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&(number.get(), id.as_str())).expect("typed cursor serializes"))
}

fn invalid_query() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error":"invalid query"})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{body::Body, extract::Extension, http::Request};
    use serde::Deserialize;
    use silverhand_backend_core::{
        conversation_messaging::messaging::task::{
            TaskBoardColumnPage, TaskBoardCounts, TaskBoardFacets, TaskBoardFilters,
        },
        foundation::{persistence::PersistenceFailure, wire_number::JsSafeInteger},
    };
    use tower::ServiceExt;

    use super::*;

    #[derive(Deserialize)]
    struct CanonicalizationFixture {
        cursor: Vec<CursorCase>,
        canonical_query: Vec<QueryCase>,
    }

    #[derive(Deserialize)]
    struct CursorCase {
        name: String,
        raw: String,
        accepted: bool,
    }

    #[derive(Deserialize)]
    struct QueryCase {
        raw: String,
        accepted: bool,
        value: Option<usize>,
    }

    #[derive(Clone)]
    struct CountingRead(Arc<AtomicUsize>);

    impl HumanTaskBoardRead for CountingRead {
        async fn column(
            &self,
            _request: &TaskBoardColumnRequest,
        ) -> Result<TaskBoardColumnPage, PersistenceFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(TaskBoardColumnPage {
                rows: Vec::new(),
                has_more: false,
            })
        }

        async fn counts(
            &self,
            _filters: &TaskBoardFilters,
        ) -> Result<TaskBoardCounts, PersistenceFailure> {
            Ok(TaskBoardCounts {
                todo: JsSafeInteger::new(0).unwrap(),
                in_progress: JsSafeInteger::new(0).unwrap(),
                in_review: JsSafeInteger::new(0).unwrap(),
                done: JsSafeInteger::new(0).unwrap(),
                closed: JsSafeInteger::new(0).unwrap(),
            })
        }

        async fn facets(
            &self,
            _viewer: &UserId,
            _container: Option<TaskBoardContainer>,
        ) -> Result<TaskBoardFacets, PersistenceFailure> {
            Ok(TaskBoardFacets {
                assignees: Vec::new(),
                creators: Vec::new(),
                has_unassigned: false,
            })
        }
    }

    #[test]
    fn public_query_strips_unknowns_and_uses_the_first_duplicate_value() {
        let values = query_map(
            Some("status=todo&future=x&status=done&limit=&limit=40"),
            &["status", "limit"],
        );
        assert_eq!(values.get("status").map(String::as_str), Some("todo"));
        assert_eq!(values.get("limit").map(String::as_str), Some("40"));
        assert!(!values.contains_key("future"));
    }

    #[test]
    fn person_filters_require_an_exact_kind_and_identifier_pair() {
        let viewer = UserId::parse("viewer").unwrap();
        for raw in [
            "assignee_type=agent",
            "assignee_id=agent-1",
            "assignee_type=unassigned&assignee_id=agent-1",
            "creator_type=user",
        ] {
            let values = query_map(
                Some(raw),
                &["assignee_type", "assignee_id", "creator_type", "creator_id"],
            );
            assert!(filters(&values, viewer.clone()).is_err(), "{raw}");
        }
    }

    #[test]
    fn cursor_codec_round_trips_and_rejects_non_domain_coordinates() {
        let id = TaskId::parse("task-7").unwrap();
        let number = TaskNumber::new(7).unwrap();
        assert_eq!(
            decode_cursor(&encode_cursor(number, &id)),
            Some(TaskBoardCursor {
                task_number: number,
                task_id: id,
            })
        );
        assert!(decode_cursor("not-base64!").is_none());
        assert!(decode_cursor(&URL_SAFE_NO_PAD.encode(br#"[0,"task-7"]"#)).is_none());
    }

    #[test]
    fn production_decoder_consumes_shared_canonical_unpadded_cursor_fixture() {
        let fixture: CanonicalizationFixture = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/shared/tests/fixtures/task-schedule-wire-canonicalization.json"
        )))
        .unwrap();
        for case in fixture.cursor {
            assert_eq!(
                decode_cursor(&case.raw).is_some(),
                case.accepted,
                "{}",
                case.name
            );
        }
    }

    #[tokio::test]
    async fn column_route_consumes_canonical_decimal_limit_before_the_read_port() {
        let fixture: CanonicalizationFixture = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/shared/tests/fixtures/task-schedule-wire-canonicalization.json"
        )))
        .unwrap();
        for case in fixture.canonical_query {
            let encoded: String = form_urlencoded::byte_serialize(case.raw.as_bytes()).collect();
            let calls = Arc::new(AtomicUsize::new(0));
            let app = human_task_board_router(HumanTaskBoardState {
                read: CountingRead(Arc::clone(&calls)),
            })
            .layer(Extension(UserId::parse("user-1").unwrap()));
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!(
                            "/api/tasks/board/column?status=todo&limit={encoded}"
                        ))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let accepted = case.accepted && case.value.is_some_and(|value| value <= MAX_LIMIT);
            assert_eq!(
                response.status(),
                if accepted {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_REQUEST
                },
                "{:?}",
                case.raw
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                usize::from(accepted),
                "{:?}",
                case.raw
            );
        }
    }
}
