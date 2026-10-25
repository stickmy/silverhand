//! Authenticated HTTP adapter for human collaboration search.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        MessageId, PrincipalIdentity,
        collaboration::human_search::{
            BoundHumanSearchPage, HumanSearchConversationPlacement, HumanSearchMillis,
            HumanSearchOrder, HumanSearchQuery, HumanSearchReadPort, HumanSearchRefusal,
            HumanSearchResult, HumanSearchStream, HumanSearchText, HumanSearchWorkspaceRef,
            IdentitySearchCategory, IdentitySearchGroups, IdentitySearchStatus, SearchIdentity,
        },
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
};

use super::{
    conversation_message_window::message,
    query_number::{CoercedInteger, NumberIssue, coerce_integer},
};
use crate::foundation::http_error::{coded_error, json_error as error};

pub const HUMAN_SEARCH_PATH: &str = "/api/search";

struct HumanSearchState<P>(Arc<P>);

impl<P> Clone for HumanSearchState<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<P>(port: P) -> Router
where
    P: HumanSearchReadPort + Send + Sync + 'static,
{
    Router::new()
        .route(HUMAN_SEARCH_PATH, get(search::<P>))
        .with_state(HumanSearchState(Arc::new(port)))
}

async fn search<P: HumanSearchReadPort>(
    State(state): State<HumanSearchState<P>>,
    principal: Option<Extension<UserId>>,
    Query(wire): Query<HumanSearchQueryWire>,
) -> Response {
    let Some(Extension(viewer)) = principal else {
        tracing::error!("human-search route reached without human principal");
        return internal();
    };
    let query = match admitted_query(&wire) {
        Ok(query) => query,
        Err(error) => return invalid_query(error),
    };
    match state.0.search(&viewer, query.clone()).await {
        Ok(Ok(page)) => match page.bind(&viewer, &query) {
            Ok(bound) => match success(&bound) {
                Ok(response) => response,
                Err(error) => {
                    tracing::error!(
                        error_chain = crate::foundation::safe_log_field::safe_display(
                            &(crate::foundation::error::describe(&error))
                        ),
                        "human-search wire projection failed"
                    );
                    internal()
                }
            },
            Err(error) => {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&error))
                    ),
                    "human-search identity seal failed"
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
                "human-search projection failed"
            );
            internal()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct HumanSearchQueryWire {
    q: Option<String>,
    include_identities: Option<String>,
    stream: Option<String>,
    workspace_id: Option<String>,
    conversation_id: Option<String>,
    thread_root_id: Option<String>,
    user_id: Option<String>,
    agent_id: Option<String>,
    from: Option<String>,
    to: Option<String>,
    order: Option<String>,
    limit: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueryIssue {
    path: &'static str,
    message: &'static str,
}

#[tracing::instrument(
    target = "silverhand::diagnostics",
    name = "search.admit",
    skip_all,
    fields(
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        operation_finished = tracing::field::Empty,
    )
)]
fn admitted_query(wire: &HumanSearchQueryWire) -> Result<HumanSearchQuery, Vec<QueryIssue>> {
    let observation = silverhand_backend_observability::Operation::current();
    let result = (|| {
        let include_identities = match wire.include_identities.as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            _ => {
                return Err(vec![QueryIssue {
                    path: "include_identities",
                    message: "expected true or false",
                }]);
            }
        };
        let text = HumanSearchText::parse(wire.q.as_deref().unwrap_or("")).map_err(|_| {
            vec![QueryIssue {
                path: "q",
                message: "String must contain at most 200 character(s)",
            }]
        })?;
        let stream = match wire.stream.as_deref().unwrap_or("all") {
            "all" => HumanSearchStream::All,
            "mainline" => HumanSearchStream::Mainline,
            "thread" => HumanSearchStream::Thread,
            _ => {
                return Err(vec![QueryIssue {
                    path: "stream",
                    message: "Invalid enum value. Expected 'all' | 'mainline' | 'thread'",
                }]);
            }
        };
        let workspace = optional_string(wire.workspace_id.as_deref())
            .map(WorkspaceId::parse)
            .transpose()
            .map_err(|_| invalid_string("workspace_id"))?;
        let conversation = optional_string(wire.conversation_id.as_deref())
            .map(ConversationId::parse)
            .transpose()
            .map_err(|_| invalid_string("conversation_id"))?;
        let thread_root = optional_string(wire.thread_root_id.as_deref())
            .map(MessageId::parse)
            .transpose()
            .map_err(|_| invalid_string("thread_root_id"))?;
        let user = optional_string(wire.user_id.as_deref())
            .map(UserId::parse)
            .transpose()
            .map_err(|_| invalid_string("user_id"))?;
        let agent = optional_string(wire.agent_id.as_deref())
            .map(AgentId::parse)
            .transpose()
            .map_err(|_| invalid_string("agent_id"))?;
        if user.is_some() && agent.is_some() {
            return Err(vec![QueryIssue {
                path: "user_id",
                message: "user_id and agent_id cannot both be set",
            }]);
        }
        let sender = user
            .map(PrincipalIdentity::Human)
            .or_else(|| agent.map(PrincipalIdentity::Agent));
        let from = optional_millis(wire.from.as_deref(), "from")?;
        let to = optional_millis(wire.to.as_deref(), "to")?;
        let order = match wire.order.as_deref().unwrap_or("recency_desc") {
            "recency_desc" => HumanSearchOrder::RecencyDesc,
            "seq_asc" => HumanSearchOrder::SequenceAsc,
            _ => {
                return Err(vec![QueryIssue {
                    path: "order",
                    message: "Invalid enum value. Expected 'recency_desc' | 'seq_asc'",
                }]);
            }
        };
        let limit = match wire.limit.as_deref() {
            None => 80,
            Some(raw) => match coerce_integer(raw) {
                Ok(CoercedInteger::Persisted(value)) if (1..=500).contains(&value) => value as u16,
                Ok(CoercedInteger::Persisted(value)) if value <= 0 => {
                    return Err(vec![QueryIssue {
                        path: "limit",
                        message: "Number must be greater than 0",
                    }]);
                }
                Ok(CoercedInteger::Persisted(_) | CoercedInteger::BeyondPersistedRange) => {
                    return Err(vec![QueryIssue {
                        path: "limit",
                        message: "Number must be less than or equal to 500",
                    }]);
                }
                Err(NumberIssue::Float) => {
                    return Err(vec![QueryIssue {
                        path: "limit",
                        message: "Expected integer, received float",
                    }]);
                }
                Err(NumberIssue::Nan | NumberIssue::NonFinite) => {
                    return Err(vec![QueryIssue {
                        path: "limit",
                        message: "Expected number, received nan",
                    }]);
                }
            },
        };

        let mut issues = Vec::new();
        if !matches!(stream, HumanSearchStream::All) && conversation.is_none() {
            issues.push(QueryIssue {
                path: "stream",
                message: "stream filter requires conversation_id",
            });
        }
        if matches!(stream, HumanSearchStream::Thread) && thread_root.is_none() {
            issues.push(QueryIssue {
                path: "thread_root_id",
                message: "stream=thread requires thread_root_id",
            });
        }
        if order == HumanSearchOrder::SequenceAsc && conversation.is_none() {
            issues.push(QueryIssue {
                path: "order",
                message: "order=seq_asc requires conversation_id",
            });
        }
        if !issues.is_empty() {
            return Err(issues);
        }
        HumanSearchQuery::admitted(
            text,
            stream,
            thread_root,
            workspace,
            conversation,
            sender,
            from,
            to,
            order,
            limit,
        )
        .map(|query| query.with_identities(include_identities))
        .map_err(|_| {
            vec![QueryIssue {
                path: "",
                message: "invalid search query",
            }]
        })
    })();
    observation.finish(&result);
    result
}

fn optional_string(raw: Option<&str>) -> Option<String> {
    raw.map(crate::conversation_messaging::messaging::admission::ecmascript_trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_millis(
    raw: Option<&str>,
    path: &'static str,
) -> Result<Option<HumanSearchMillis>, Vec<QueryIssue>> {
    let Some(raw) = raw else { return Ok(None) };
    // SearchOptionalMillisSchema only preprocesses the exact empty string to
    // absent. Any nonempty spelling, including ECMAScript whitespace, reaches
    // `Number(value)` and therefore coerces to zero.
    if raw.is_empty() {
        return Ok(None);
    }
    let value = match coerce_integer(raw) {
        Ok(CoercedInteger::Persisted(value)) if value >= 0 => value as f64,
        Ok(CoercedInteger::BeyondPersistedRange) => 1e20,
        Ok(CoercedInteger::Persisted(_)) => {
            return Err(vec![QueryIssue {
                path,
                message: "Number must be greater than or equal to 0",
            }]);
        }
        Err(NumberIssue::Float) => {
            return Err(vec![QueryIssue {
                path,
                message: "Expected integer, received float",
            }]);
        }
        Err(NumberIssue::Nan | NumberIssue::NonFinite) => {
            return Err(vec![QueryIssue {
                path,
                message: "Expected number, received nan",
            }]);
        }
    };
    HumanSearchMillis::from_ecmascript_number(value)
        .map(Some)
        .map_err(|_| {
            vec![QueryIssue {
                path,
                message: "Expected number, received nan",
            }]
        })
}

fn invalid_string(path: &'static str) -> Vec<QueryIssue> {
    vec![QueryIssue {
        path,
        message: "Expected string, received invalid value",
    }]
}

fn invalid_query(issues: Vec<QueryIssue>) -> Response {
    let mut field_errors: Map<String, Value> = Map::new();
    let mut form_errors = Vec::new();
    for issue in issues {
        if issue.path.is_empty() {
            form_errors.push(Value::String(issue.message.to_owned()));
        } else {
            field_errors
                .entry(issue.path.to_owned())
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .expect("field errors are arrays")
                .push(Value::String(issue.message.to_owned()));
        }
    }
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "formErrors": form_errors,
                "fieldErrors": field_errors,
            }
        })),
    )
        .into_response()
}

#[tracing::instrument(
    target = "silverhand::diagnostics",
    name = "search.wire",
    skip_all,
    fields(
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        operation_finished = tracing::field::Empty,
    )
)]
fn success(
    page: &BoundHumanSearchPage,
) -> Result<Response, super::conversation_message_window::MessageWindowWireError> {
    let observation = silverhand_backend_observability::Operation::current();
    let result = (|| {
        let results = page
            .results()
            .iter()
            .map(result)
            .collect::<Result<Vec<_>, _>>()?;
        let mut body = json!({
            "query": query_wire(page.query()),
            "results": results,
            "total": page.total().get(),
            "truncated": page.truncated(),
        });
        if let Some(identities) = page.identities() {
            body["identities"] = identities_wire(identities);
        }
        Ok(Json(body).into_response())
    })();
    observation.finish(&result);
    result
}

fn identities_wire(groups: &IdentitySearchGroups) -> Value {
    let status = match groups.status() {
        IdentitySearchStatus::Searched => "searched",
        IdentitySearchStatus::Filtered => "filtered",
        IdentitySearchStatus::EmptyQuery => "empty_query",
    };
    json!({
        "status": status,
        "humans": identity_category_wire(groups.humans()),
        "agents": identity_category_wire(groups.agents()),
    })
}

fn identity_category_wire(group: &IdentitySearchCategory) -> Value {
    let results: Vec<_> = group.results().iter().map(identity_wire).collect();
    json!({
        "results": results,
        "total": group.total(),
        "truncated": group.total() > group.results().len(),
    })
}

fn identity_wire(row: &SearchIdentity) -> Value {
    use crate::conversation_messaging::messaging::author_wire::principal_avatar_url;
    use silverhand_backend_core::conversation_messaging::messaging::{
        PrincipalIdentity, PrincipalKind,
    };
    let (kind, principal_kind) = match row.identity {
        PrincipalIdentity::Human(_) => ("human", PrincipalKind::Human),
        PrincipalIdentity::Agent(_) => ("agent", PrincipalKind::Agent),
    };
    json!({
        "kind": kind,
        "id": row.identity.as_str(),
        "name": row.name,
        "display_name": row.display_name,
        "avatar_url": principal_avatar_url(principal_kind, row.identity.as_str(), row.avatar),
        "runtime": row.runtime.map(|runtime| runtime.as_str()),
    })
}

fn result(
    result: &HumanSearchResult,
) -> Result<Value, super::conversation_message_window::MessageWindowWireError> {
    match result {
        HumanSearchResult::Workspace(selected) => Ok(json!({
            "type": "workspace",
            "workspace": workspace_wire(selected.workspace(), Some(selected)),
        })),
        HumanSearchResult::Conversation(selected) => {
            let mut conversation = conversation_wire(selected.conversation());
            conversation["archived_at"] = json!(selected.archived_at());
            Ok(json!({ "type": "conversation", "conversation": conversation }))
        }
        HumanSearchResult::Message(selected) => Ok(json!({
            "type": "message",
            "conversation": conversation_wire(selected.conversation()),
            "message": message(selected.message().projection())?,
        })),
    }
}

fn workspace_wire(
    workspace: &HumanSearchWorkspaceRef,
    selected: Option<&silverhand_backend_core::conversation_messaging::messaging::collaboration::human_search::SelectedHumanSearchWorkspace>,
) -> Value {
    let mut value = json!({
        "id": workspace.id().as_str(),
        "name": workspace.name(),
        "owner_agent_id": workspace.owner_agent().map(|id| id.as_str()),
    });
    if let Some(selected) = selected {
        let object = value.as_object_mut().expect("workspace wire is an object");
        object.insert("description".to_owned(), json!(selected.description()));
        object.insert("archived_at".to_owned(), json!(selected.archived_at()));
        object.insert("created_at".to_owned(), json!(selected.created_at()));
        object.insert("updated_at".to_owned(), json!(selected.updated_at()));
    }
    value
}

fn conversation_wire(
    conversation: &silverhand_backend_core::conversation_messaging::messaging::collaboration::human_search::HumanSearchConversationRef,
) -> Value {
    let mut value = json!({
        "id": conversation.id().as_str(),
        "title": conversation.title(),
        "roster_policy": conversation.roster().as_str(),
        "channel_id": conversation.channel_id().as_str(),
        "closed_at": conversation.closed_at(),
        "created_at": conversation.created_at(),
    });
    let object = value
        .as_object_mut()
        .expect("conversation wire is an object");
    match conversation.placement() {
        HumanSearchConversationPlacement::Workspace { workspace, .. } => {
            object.insert("placement_kind".to_owned(), json!("workspace"));
            object.insert("workspace".to_owned(), workspace_wire(workspace, None));
        }
        HumanSearchConversationPlacement::DirectPair => {
            object.insert("placement_kind".to_owned(), json!("direct_pair"));
        }
    }
    value
}

fn query_wire(query: &HumanSearchQuery) -> Value {
    let mut value = Map::new();
    if query.include_identities() {
        value.insert("include_identities".to_owned(), json!(true));
    }
    value.insert("q".to_owned(), json!(query.text().as_str()));
    value.insert("stream".to_owned(), json!(query.stream().as_str()));
    if let Some(workspace) = query.workspace() {
        value.insert("workspace_id".to_owned(), json!(workspace.as_str()));
    }
    if let Some(conversation) = query.conversation() {
        value.insert("conversation_id".to_owned(), json!(conversation.as_str()));
    }
    if let Some(root) = query.thread_root() {
        value.insert("thread_root_id".to_owned(), json!(root.as_str()));
    }
    if let Some(sender) = query.sender() {
        let key = match sender {
            PrincipalIdentity::Human(_) => "user_id",
            PrincipalIdentity::Agent(_) => "agent_id",
        };
        value.insert(key.to_owned(), json!(sender.as_str()));
    }
    if let Some(from) = query.from() {
        value.insert("from".to_owned(), millis_wire(from));
    }
    if let Some(to) = query.to() {
        value.insert("to".to_owned(), millis_wire(to));
    }
    value.insert("order".to_owned(), json!(query.order().as_str()));
    value.insert("limit".to_owned(), json!(query.limit()));
    Value::Object(value)
}

fn millis_wire(value: HumanSearchMillis) -> Value {
    value
        .persisted_value()
        .map_or_else(|| json!(1e20), |value| json!(value))
}

fn refusal_wire(refusal: HumanSearchRefusal) -> Response {
    match refusal {
        HumanSearchRefusal::ThreadRootNotFound => coded_error(
            StatusCode::NOT_FOUND,
            "thread root message not found in this conversation",
            "thread_root_not_found",
        ),
    }
}

fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[cfg(test)]
mod tests;
