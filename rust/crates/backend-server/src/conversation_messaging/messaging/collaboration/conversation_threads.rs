//! Authenticated HTTP adapter for one conversation's reply-cluster list.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageSeq,
        collaboration::conversation_threads::{
            BoundConversationThreadsPage, ConversationThreadsReadPort, ConversationThreadsRefusal,
            ThreadListCursor, ThreadListFilter, ThreadListQuery,
        },
    },
    foundation::identity::{ConversationId, UserId},
};

use crate::conversation_messaging::messaging::author_wire::author_dto;

use super::query_number::{CoercedInteger, coerce_integer};
use crate::foundation::http_error::{coded_error, json_error as error};

pub const CONVERSATION_THREADS_PATH: &str = "/api/conversations/{id}/threads";

struct ThreadsState<P>(Arc<P>);

impl<P> Clone for ThreadsState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: ConversationThreadsReadPort + Send + Sync + 'static,
{
    Router::new()
        .route(CONVERSATION_THREADS_PATH, get(read::<P>))
        .with_state(ThreadsState(Arc::new(port)))
}

async fn read<P: ConversationThreadsReadPort>(
    State(state): State<ThreadsState<P>>,
    principal: Option<Extension<UserId>>,
    Path(raw_conversation): Path<String>,
    Query(raw_query): Query<ThreadsQuery>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("conversation-threads route reached without human principal");
        return internal();
    };
    let conversation = match ConversationId::parse(raw_conversation) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "channel id required"),
    };

    // The TypeScript route establishes participation before parsing pagination.
    // This preflight preserves that rejection order, but is not an authority
    // token: `read` re-evaluates access in its own snapshot.
    match state.0.admit(&viewer, &conversation).await {
        Ok(Ok(())) => {}
        Ok(Err(refusal)) => return refusal_wire(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "conversation-threads admission failed"
            );
            return internal();
        }
    }

    let query = match page_query(&raw_query) {
        Ok(value) => value,
        Err(QueryRefusal::Limit) => {
            return coded_error(StatusCode::BAD_REQUEST, "invalid limit", "invalid_limit");
        }
        Err(QueryRefusal::Cursor) => {
            return coded_error(StatusCode::BAD_REQUEST, "invalid cursor", "invalid_cursor");
        }
        Err(QueryRefusal::Filter) => {
            return coded_error(StatusCode::BAD_REQUEST, "invalid filter", "invalid_filter");
        }
    };

    match state.0.read(&viewer, &conversation, query).await {
        Ok(Ok(page)) => match page.bind(&viewer, &conversation, query) {
            Ok(page) => success(&page),
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "conversation-threads identity seal failed"
                );
                internal()
            }
        },
        Ok(Err(refusal)) => refusal_wire(refusal),
        Err(error) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&error))
                ),
                "conversation-threads projection failed"
            );
            internal()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ThreadsQuery {
    limit: Option<String>,
    cursor: Option<String>,
    filter: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueryRefusal {
    Limit,
    Cursor,
    Filter,
}

fn page_query(raw: &ThreadsQuery) -> Result<ThreadListQuery, QueryRefusal> {
    let limit = match raw.limit.as_deref() {
        None => 30,
        Some(value) => match coerce_integer(value).map_err(|_| QueryRefusal::Limit)? {
            CoercedInteger::Persisted(value) if value >= 1 => value.min(50) as u16,
            CoercedInteger::BeyondPersistedRange => 50,
            CoercedInteger::Persisted(_) => return Err(QueryRefusal::Limit),
        },
    };
    let before = match raw.cursor.as_deref() {
        None | Some("") => None,
        Some(value) => Some(decode_cursor(value)?),
    };
    let filter = match raw.filter.as_deref().unwrap_or("all") {
        "all" => ThreadListFilter::All,
        "followed-unread" => ThreadListFilter::FollowedUnread,
        _ => return Err(QueryRefusal::Filter),
    };
    ThreadListQuery::new(limit, before, filter).map_err(|_| QueryRefusal::Limit)
}

fn decode_cursor(raw: &str) -> Result<ThreadListCursor, QueryRefusal> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| QueryRefusal::Cursor)?;
    let [last_reply_at, root_seq]: [i64; 2] =
        serde_json::from_slice(&bytes).map_err(|_| QueryRefusal::Cursor)?;
    let root_seq = MessageSeq::try_from(root_seq).map_err(|_| QueryRefusal::Cursor)?;
    ThreadListCursor::new(last_reply_at, root_seq).map_err(|_| QueryRefusal::Cursor)
}

fn encode_cursor(cursor: ThreadListCursor) -> String {
    URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&[cursor.last_reply_at(), cursor.root_seq().get()])
            .expect("two integers always serialize"),
    )
}

fn success(page: &BoundConversationThreadsPage) -> Response {
    let threads: Vec<Value> = page
        .threads()
        .iter()
        .map(|selected| {
            let entry = selected.entry();
            json!({
                "root_message_id": entry.root_id().as_str(),
                "root_author": entry.root_author().map(author_dto).unwrap_or(Value::Null),
                "root_preview": entry.root_preview(),
                "root_created_at": entry.root_created_at_ms(),
                "root_seq": entry.root_seq().get(),
                "reply_count": entry.reply_count(),
                "last_reply_at": entry.last_reply_at_ms(),
                "last_reply_message_id": entry.last_reply_message_id().as_str(),
                "repliers": entry.repliers().iter().map(author_dto).collect::<Vec<_>>(),
                "replier_total": entry.replier_total(),
                "viewer_following": selected.viewer_following(),
            })
        })
        .collect();
    Json(json!({
        "threads": threads,
        "next_cursor": page.next_cursor().map(encode_cursor),
    }))
    .into_response()
}

fn refusal_wire(refusal: ConversationThreadsRefusal) -> Response {
    match refusal {
        ConversationThreadsRefusal::NotFound => error(StatusCode::NOT_FOUND, "channel not found"),
        ConversationThreadsRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
    }
}

fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[cfg(test)]
mod tests;
