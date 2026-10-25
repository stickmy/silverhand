//! Human by-number summary and by-id detail reads.

use std::{collections::HashMap, sync::Arc};

use axum::{
    Json, Router,
    extract::{Extension, Path, RawQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use silverhand_backend_core::conversation_messaging::messaging::task::{
    ReadViewer, TaskAuthorizedPointRead, TaskReadActor, TaskReadAddress, TaskReadRequest,
};
use silverhand_backend_core::foundation::identity::{ConversationId, UserId};

use super::wire::{project_detail, project_summary};
use crate::foundation::http_error::plain_500 as safe_500;

pub struct HumanTaskPointState<R> {
    pub read: R,
}

pub fn human_task_point_router<R>(state: HumanTaskPointState<R>) -> Router
where
    R: TaskAuthorizedPointRead + Send + Sync + 'static,
{
    Router::new()
        .route("/api/tasks/by-number", get(by_number::<R>))
        .route("/api/tasks/{taskId}", get(by_id::<R>))
        .with_state(Arc::new(state))
}

async fn by_number<R>(
    State(state): State<Arc<HumanTaskPointState<R>>>,
    principal: Option<Extension<UserId>>,
    RawQuery(raw): RawQuery,
) -> Response
where
    R: TaskAuthorizedPointRead + Send + Sync + 'static,
{
    let Some(Extension(viewer)) = principal else {
        return safe_500();
    };
    let Ok(values) = query_map(raw.as_deref()) else {
        return invalid_query();
    };
    let Some(conversation) = values
        .get("conversation_id")
        .and_then(|v| ConversationId::parse(v).ok())
    else {
        return invalid_query();
    };
    let Some(number) = values
        .get("task_number")
        .and_then(|v| super::wire_number::parse_canonical_task_number(v))
    else {
        return invalid_query();
    };
    let point = match state
        .read
        .read_authorized_point_by_conversation_number(viewer.clone(), conversation, number)
        .await
    {
        Ok(Some(point)) => point,
        Ok(None) => return not_found(),
        Err(_) => return safe_500(),
    };
    let Some(source) = point.for_viewer(&ReadViewer::User(viewer)) else {
        return safe_500();
    };
    match project_summary(source.summary) {
        Ok(task) => (StatusCode::OK, Json(serde_json::json!({"task":task}))).into_response(),
        Err(_) => safe_500(),
    }
}

async fn by_id<R>(
    State(state): State<Arc<HumanTaskPointState<R>>>,
    Path(raw): Path<String>,
    principal: Option<Extension<UserId>>,
) -> Response
where
    R: TaskAuthorizedPointRead + Send + Sync + 'static,
{
    let Some(Extension(viewer)) = principal else {
        return safe_500();
    };
    let Ok(id) =
        silverhand_backend_core::conversation_messaging::messaging::task::TaskId::parse(raw)
    else {
        return not_found();
    };
    let point = match read(&state, TaskReadAddress::ById(id), viewer.clone()).await {
        Ok(Some(point)) => point,
        Ok(None) => return not_found(),
        Err(()) => return safe_500(),
    };
    let Some(source) = point.for_viewer(&ReadViewer::User(viewer)) else {
        return safe_500();
    };
    match project_detail(source) {
        Ok(task) => (StatusCode::OK, Json(serde_json::json!({"task":task}))).into_response(),
        Err(_) => safe_500(),
    }
}

async fn read<R>(
    state: &Arc<HumanTaskPointState<R>>,
    address: TaskReadAddress,
    viewer: UserId,
) -> Result<
    Option<silverhand_backend_core::conversation_messaging::messaging::task::VisibleTaskPoint>,
    (),
>
where
    R: TaskAuthorizedPointRead + Send + Sync + 'static,
{
    let request = TaskReadRequest {
        address,
        actor: TaskReadActor::User(viewer),
    };
    state
        .read
        .read_authorized_point(request)
        .await
        .map_err(|_| ())
}

fn query_map(raw: Option<&str>) -> Result<HashMap<String, String>, ()> {
    let mut values = HashMap::new();
    for (key, value) in form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        if matches!(key.as_ref(), "conversation_id" | "task_number") {
            values
                .entry(key.into_owned())
                .or_insert_with(|| value.into_owned());
        }
    }
    if values.len() == 2 {
        Ok(values)
    } else {
        Err(())
    }
}

fn invalid_query() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error":"invalid query"})),
    )
        .into_response()
}
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error":"task not found","code":"not_found"})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{body::Body, extract::Extension, http::Request};
    use serde::Deserialize;
    use silverhand_backend_core::{
        conversation_messaging::messaging::task::VisibleTaskPoint,
        foundation::persistence::PersistenceFailure,
    };
    use tower::ServiceExt;

    use super::*;

    #[derive(Deserialize)]
    struct Fixture {
        canonical_query: Vec<QueryCase>,
    }

    #[derive(Deserialize)]
    struct QueryCase {
        raw: String,
        accepted: bool,
        value: Option<i64>,
    }

    #[derive(Clone)]
    struct RecordingRead(Arc<Mutex<Vec<i64>>>);

    impl TaskAuthorizedPointRead for RecordingRead {
        async fn read_authorized_point(
            &self,
            _request: TaskReadRequest,
        ) -> Result<Option<VisibleTaskPoint>, PersistenceFailure> {
            unreachable!("the by-number route uses the snapshot-bound method")
        }

        async fn read_authorized_point_by_conversation_number(
            &self,
            _viewer: UserId,
            _conversation: ConversationId,
            number: silverhand_backend_core::conversation_messaging::messaging::task::TaskNumber,
        ) -> Result<Option<VisibleTaskPoint>, PersistenceFailure> {
            self.0.lock().unwrap().push(number.get());
            Ok(None)
        }
    }

    #[test]
    fn by_number_query_is_strip_unknown_first_duplicate_and_requires_both_axes() {
        let values = query_map(Some(
            "future=x&conversation_id=c1&conversation_id=c2&task_number=4",
        ))
        .unwrap();
        assert_eq!(
            values.get("conversation_id").map(String::as_str),
            Some("c1")
        );
        assert_eq!(values.get("task_number").map(String::as_str), Some("4"));
        assert!(!values.contains_key("future"));
        assert!(query_map(Some("conversation_id=c1")).is_err());
    }

    #[tokio::test]
    async fn by_number_route_consumes_canonical_decimal_corpus_before_the_read_port() {
        let fixture: Fixture = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/shared/tests/fixtures/task-schedule-wire-canonicalization.json"
        )))
        .unwrap();
        for case in fixture.canonical_query {
            let encoded: String = form_urlencoded::byte_serialize(case.raw.as_bytes()).collect();
            let numbers = Arc::new(Mutex::new(Vec::new()));
            let app = human_task_point_router(HumanTaskPointState {
                read: RecordingRead(Arc::clone(&numbers)),
            })
            .layer(Extension(UserId::parse("user-1").unwrap()));
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!(
                            "/api/tasks/by-number?conversation_id=conv-1&task_number={encoded}"
                        ))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if case.accepted {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_REQUEST
                },
                "{:?}",
                case.raw
            );
            assert_eq!(
                *numbers.lock().unwrap(),
                if case.accepted {
                    vec![case.value.expect("accepted corpus value")]
                } else {
                    Vec::new()
                },
                "{:?}",
                case.raw
            );
        }
    }
}
