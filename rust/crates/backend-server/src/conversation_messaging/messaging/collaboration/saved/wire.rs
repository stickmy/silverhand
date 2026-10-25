use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value, json};
use silverhand_backend_core::conversation_messaging::messaging::{
    FromKind,
    attachments::AttachmentSnapshot,
    collaboration::saved::{
        BoundSavedEntry, SaveExecution, SavedAction, SavedEntry, SavedPlace, SavedReadRefusal,
        UnsaveExecution,
    },
};

use crate::conversation_messaging::messaging::author_wire::author_dto;
pub(super) use crate::foundation::http_error::plain_500 as internal_error_response;

pub const SAVED_CHANGED_EVENT: &str = "saved_changed";

pub fn changed_event(
    fact: &silverhand_backend_core::conversation_messaging::messaging::collaboration::saved::SavedChangeFact,
) -> Value {
    json!({
        "message_id": fact.message().as_str(),
        "action": match fact.action() { SavedAction::Save => "save", SavedAction::Unsave => "unsave" },
    })
}

pub(super) fn list_success(entries: &[BoundSavedEntry]) -> Response {
    Json(json!({
        "saved": entries.iter().map(saved_entry).collect::<Vec<_>>()
    }))
    .into_response()
}

fn audit_publication_failure(
    failure: Option<
        &silverhand_backend_core::conversation_messaging::messaging::collaboration::saved::SavedPublicationFailure,
    >,
) {
    if let Some(failure) = failure {
        tracing::error!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &(crate::foundation::error::describe(failure))
            ),
            "saved-message publication failed"
        );
    }
}

pub(super) fn save_success(execution: SaveExecution) -> Response {
    audit_publication_failure(execution.publication_failure());
    (
        StatusCode::CREATED,
        Json(json!({"saved": saved_entry(execution.committed().entry())})),
    )
        .into_response()
}

pub(super) fn unsave_success(execution: UnsaveExecution) -> Response {
    audit_publication_failure(execution.publication_failure());
    StatusCode::NO_CONTENT.into_response()
}

fn saved_entry(bound: &BoundSavedEntry) -> Value {
    let entry: &SavedEntry = bound.entry();
    let projection = entry.message().projection();
    let metadata = projection
        .metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    json!({
        "id": entry.id().as_str(),
        "message": {
            "id": projection.id.as_str(),
            "conversation_id": projection.conversation.as_str(),
            "from_kind": from_kind(projection.from_kind),
            "from_agent_id": projection.from_agent.as_ref().map(|id| id.as_str()),
            "from_user_id": projection.from_user.as_ref().map(|id| id.as_str()),
            "author": projection.author.as_ref().map(author_dto),
            "content": projection.content,
            "metadata": metadata,
            "attachments": attachment_list(&projection.attachments),
            "reactions": Value::Null,
            "seq": projection.seq.get(),
            "created_at": projection.created_at_ms,
            "thread_root_id": projection.thread_root.as_ref().map(|id| id.as_str()),
            "linked_task": projection.linked_task.as_ref().map(linked_task),
        },
        "place": place(entry.place()),
        "saved_at": entry.saved_at().get(),
    })
}

fn from_kind(kind: FromKind) -> &'static str {
    match kind {
        FromKind::User => "user",
        FromKind::Agent => "agent",
        FromKind::System => "system",
    }
}

fn place(place: &SavedPlace) -> Value {
    match place {
        SavedPlace::WorkspaceChannel {
            conversation,
            workspace,
            channel_name,
        } => json!({
            "kind": "workspace_channel",
            "conversation_id": conversation.as_str(),
            "workspace_id": workspace.as_str(),
            "channel_name": channel_name,
        }),
        SavedPlace::DirectPair { conversation } => json!({
            "kind": "direct_pair",
            "conversation_id": conversation.as_str(),
        }),
    }
}

fn attachment_list(attachments: &[AttachmentSnapshot]) -> Value {
    if attachments.is_empty() {
        return Value::Null;
    }
    Value::Array(
        attachments
            .iter()
            .map(|attachment| {
                let mut object = Map::new();
                object.insert("id".to_owned(), json!(attachment.id.as_str()));
                object.insert("filename".to_owned(), json!(attachment.filename));
                object.insert("size".to_owned(), json!(attachment.size));
                object.insert("content_type".to_owned(), json!(attachment.content_type));
                if let Some(label) = attachment.label {
                    object.insert("label".to_owned(), json!(label.to_string()));
                }
                Value::Object(object)
            })
            .collect(),
    )
}

fn linked_task(
    task: &silverhand_backend_core::conversation_messaging::messaging::collaboration::saved::SavedLinkedTask,
) -> Value {
    json!({
        "task_id": task.id.as_str(),
        "task_number": task.number.get(),
        "title": task.title.as_str(),
        "status": task.status.as_str(),
        "assignee": task.assignee.as_ref().map(|assignee| json!({
            "type": match assignee.principal() {
                silverhand_backend_core::conversation_messaging::messaging::PrincipalIdentity::Agent(_) => "agent",
                silverhand_backend_core::conversation_messaging::messaging::PrincipalIdentity::Human(_) => "user",
            },
            "id": assignee.principal().as_str(),
            "name": assignee.name(),
        })),
    })
}

pub(super) fn list_refusal(refusal: SavedReadRefusal) -> Response {
    match refusal {
        SavedReadRefusal::WorkspaceNotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"workspace not found"})),
        )
            .into_response(),
        SavedReadRefusal::Forbidden => {
            (StatusCode::FORBIDDEN, Json(json!({"error":"forbidden"}))).into_response()
        }
    }
}

pub(super) fn invalid_input(issues: Vec<Value>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"invalid input","issues":issues})),
    )
        .into_response()
}

pub(super) fn message_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error":"message not found"})),
    )
        .into_response()
}

pub(super) fn internal_error(message: &'static str) -> Response {
    tracing::error!("{message}");
    internal_error_response()
}
