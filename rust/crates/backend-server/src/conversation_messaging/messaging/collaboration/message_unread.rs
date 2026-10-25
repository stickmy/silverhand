//! Authenticated HTTP adapters for the three roll-up message-unread reads.

use std::sync::Arc;

use crate::foundation::http_error::json_error as error;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Map, Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::message_unread::{
        BoundConversationMessageUnread, BoundViewerMessageUnreadSummary,
        BoundWorkspaceMessageUnread, ConversationMessageUnreadReadPort,
        ConversationMessageUnreadRefusal, LatestUnreadPointer, ViewerMessageUnreadSummaryReadPort,
        WorkspaceMessageUnreadReadPort, WorkspaceMessageUnreadRefusal,
    },
    foundation::identity::{ConversationId, UserId, WorkspaceId},
};

pub const WORKSPACE_MESSAGE_UNREAD_PATH: &str = "/api/workspaces/{id}/message-unread";
pub const MESSAGE_UNREAD_SUMMARY_PATH: &str = "/api/message-unread-summary";
pub const CONVERSATION_MESSAGE_UNREAD_PATH: &str = "/api/channels/{id}/message-unread";

struct MessageUnreadState<P>(Arc<P>);

impl<P> Clone for MessageUnreadState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: WorkspaceMessageUnreadReadPort
        + ViewerMessageUnreadSummaryReadPort
        + ConversationMessageUnreadReadPort
        + Send
        + Sync
        + 'static,
{
    Router::new()
        .route(WORKSPACE_MESSAGE_UNREAD_PATH, get(workspace::<P>))
        .route(MESSAGE_UNREAD_SUMMARY_PATH, get(summary::<P>))
        .route(CONVERSATION_MESSAGE_UNREAD_PATH, get(conversation::<P>))
        .with_state(MessageUnreadState(Arc::new(port)))
}

async fn workspace<P>(
    State(state): State<MessageUnreadState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw_id): Path<String>,
) -> Response
where
    P: WorkspaceMessageUnreadReadPort + Send + Sync,
{
    let Some(Extension(viewer)) = principal else {
        tracing::error!("workspace-message-unread route reached without human principal");
        return internal();
    };
    let requested = match WorkspaceId::parse(raw_id) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "workspace id required"),
    };
    match state.0.read_workspace(&viewer, &requested).await {
        Ok(Ok(value)) => match value.bind(&requested) {
            Ok(value) => workspace_success(&value),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "workspace-message-unread identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(WorkspaceMessageUnreadRefusal::NotFound)) => {
            error(StatusCode::NOT_FOUND, "workspace not found")
        }
        Ok(Err(WorkspaceMessageUnreadRefusal::Forbidden)) => {
            error(StatusCode::FORBIDDEN, "forbidden")
        }
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "workspace-message-unread projection failed"
            );
            internal()
        }
    }
}

async fn summary<P>(
    State(state): State<MessageUnreadState<P>>,
    principal: Option<Extension<UserId>>,
) -> Response
where
    P: ViewerMessageUnreadSummaryReadPort + Send + Sync,
{
    let Some(Extension(viewer)) = principal else {
        tracing::error!("message-unread-summary route reached without human principal");
        return internal();
    };
    match state.0.read_summary(&viewer).await {
        Ok(value) => match value.bind(&viewer) {
            Ok(value) => summary_success(&value),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "message-unread-summary identity seal failed"
                );
                internal()
            }
        },
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "message-unread-summary projection failed"
            );
            internal()
        }
    }
}

async fn conversation<P>(
    State(state): State<MessageUnreadState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw_id): Path<String>,
) -> Response
where
    P: ConversationMessageUnreadReadPort + Send + Sync,
{
    let Some(Extension(viewer)) = principal else {
        tracing::error!("conversation-message-unread route reached without human principal");
        return internal();
    };
    let requested = match ConversationId::parse(raw_id) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "channel id required"),
    };
    match state.0.read_conversation(&viewer, &requested).await {
        Ok(Ok(value)) => match value.bind(&requested) {
            Ok(value) => conversation_success(&value),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "conversation-message-unread identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(ConversationMessageUnreadRefusal::NotFound)) => {
            error(StatusCode::NOT_FOUND, "channel not found")
        }
        Ok(Err(ConversationMessageUnreadRefusal::Forbidden)) => {
            error(StatusCode::FORBIDDEN, "forbidden")
        }
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "conversation-message-unread projection failed"
            );
            internal()
        }
    }
}

fn workspace_success(value: &BoundWorkspaceMessageUnread) -> Response {
    if value
        .rows()
        .iter()
        .any(|row| !safe_number(row.count()) || !safe_number(row.latest().seq().get()))
    {
        return internal();
    }
    let mut counts = Map::new();
    let mut latest = Map::new();
    for row in value.rows() {
        counts.insert(row.conversation().as_str().to_owned(), json!(row.count()));
        latest.insert(
            row.conversation().as_str().to_owned(),
            pointer_wire(row.latest()),
        );
    }
    let total = match value.total() {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "workspace-message-unread total overflowed"
            );
            return internal();
        }
    };
    if !safe_number(total) {
        return internal();
    }
    Json(json!({
        "by_conversation": counts,
        "latest_by_conversation": latest,
        "total": total,
    }))
    .into_response()
}

fn summary_success(value: &BoundViewerMessageUnreadSummary) -> Response {
    if value.rows().iter().any(|row| !safe_number(row.count())) {
        return internal();
    }
    let by_workspace = value
        .rows()
        .iter()
        .map(|row| (row.workspace().as_str().to_owned(), json!(row.count())))
        .collect::<Map<String, Value>>();
    Json(json!({"by_workspace": by_workspace})).into_response()
}

fn conversation_success(value: &BoundConversationMessageUnread) -> Response {
    if !safe_number(value.unread().count())
        || value
            .unread()
            .latest()
            .is_some_and(|pointer| !safe_number(pointer.seq().get()))
    {
        return internal();
    }
    let mut counts = Map::new();
    counts.insert(
        value.conversation().as_str().to_owned(),
        json!(value.unread().count()),
    );
    let mut latest = Map::new();
    if let Some(pointer) = value.unread().latest() {
        latest.insert(
            value.conversation().as_str().to_owned(),
            pointer_wire(pointer),
        );
    }
    Json(json!({
        "by_conversation": counts,
        "latest_by_conversation": latest,
        "total": value.unread().count(),
    }))
    .into_response()
}

fn pointer_wire(value: &LatestUnreadPointer) -> Value {
    json!({
        "conversation_id": value.conversation().as_str(),
        "message_id": value.message().as_str(),
        "seq": value.seq().get(),
        "thread_root_id": value.thread_root().map(|root| root.as_str()),
    })
}

fn safe_number(value: i64) -> bool {
    silverhand_backend_core::foundation::wire_number::JsSafeInteger::new(value).is_ok()
}

fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[cfg(test)]
mod numeric_tests {
    use super::*;
    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::message_unread::{
            ConversationMessageUnread, NonZeroUnreadCount, UnreadCount, UnreadRollup,
            ViewerMessageUnreadSummary, WorkspaceConversationUnread, WorkspaceMessageUnread,
            WorkspaceUnreadTotal,
        },
        conversation_messaging::messaging::{MessageId, MessageSeq},
        foundation::identity::{ConversationId, UserId, WorkspaceId},
    };

    #[test]
    fn actual_success_envelopes_keep_safe_max_and_reject_max_plus_one() {
        let workspace = WorkspaceId::parse("w").unwrap();
        let viewer = UserId::parse("v").unwrap();
        for (count, seq, expected, summary_expected) in [
            (
                9_007_199_254_740_991,
                9_007_199_254_740_991,
                StatusCode::OK,
                StatusCode::OK,
            ),
            (
                9_007_199_254_740_992,
                1,
                StatusCode::INTERNAL_SERVER_ERROR,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                1,
                9_007_199_254_740_992,
                StatusCode::INTERNAL_SERVER_ERROR,
                StatusCode::OK,
            ),
        ] {
            let conversation = ConversationId::parse("c").unwrap();
            let unread = UnreadRollup::projected(
                UnreadCount::new(count).unwrap(),
                Some(LatestUnreadPointer::projected(
                    conversation.clone(),
                    MessageId::parse("m").unwrap(),
                    MessageSeq::try_from(seq).unwrap(),
                    None,
                )),
            )
            .unwrap();
            let point = ConversationMessageUnread::projected(conversation.clone(), unread.clone())
                .unwrap()
                .bind(&conversation)
                .unwrap();
            assert_eq!(conversation_success(&point).status(), expected);
            let UnreadRollup::Unread(unread) = unread else {
                unreachable!()
            };
            let row =
                WorkspaceConversationUnread::projected(workspace.clone(), conversation, unread)
                    .unwrap();
            let bound = WorkspaceMessageUnread::projected(workspace.clone(), vec![row])
                .unwrap()
                .bind(&workspace)
                .unwrap();
            assert_eq!(workspace_success(&bound).status(), expected);
            let summary = ViewerMessageUnreadSummary::projected(
                viewer.clone(),
                vec![WorkspaceUnreadTotal::projected(
                    viewer.clone(),
                    workspace.clone(),
                    NonZeroUnreadCount::new(UnreadCount::new(count).unwrap()).unwrap(),
                )],
            )
            .unwrap()
            .bind(&viewer)
            .unwrap();
            assert_eq!(summary_success(&summary).status(), summary_expected);
        }
        let rows = ["c1", "c2"]
            .into_iter()
            .map(|id| {
                let conversation = ConversationId::parse(id).unwrap();
                let UnreadRollup::Unread(unread) = UnreadRollup::projected(
                    UnreadCount::new(9_007_199_254_740_991).unwrap(),
                    Some(LatestUnreadPointer::projected(
                        conversation.clone(),
                        MessageId::parse(id).unwrap(),
                        MessageSeq::try_from(1).unwrap(),
                        None,
                    )),
                )
                .unwrap() else {
                    unreachable!()
                };
                WorkspaceConversationUnread::projected(workspace.clone(), conversation, unread)
                    .unwrap()
            })
            .collect();
        let total_overflow = WorkspaceMessageUnread::projected(workspace.clone(), rows)
            .unwrap()
            .bind(&workspace)
            .unwrap();
        assert_eq!(
            workspace_success(&total_overflow).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
