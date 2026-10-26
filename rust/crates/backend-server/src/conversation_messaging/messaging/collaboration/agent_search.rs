//! `POST /internal/agents/{aid}/search-messages` — LIKE search over the agent's
//! reachable messages.
//!
//! Behind the runtime-credential perimeter (agent bound to `{aid}` first). The
//! scope is selector-only: exactly one of `current_workspace` / `workspace_id` /
//! `conversation_id`, or none (the calling conversation). The body is `.strict()`
//! (retired keys fail loudly) with an at-most-one-selector semantic check. Cross-
//! workspace and `read_messages` are gated the same way `list-channels` gates them;
//! the participation guard lives in the read (an unreachable message is never a
//! hit).

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, FromRequest, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::agent_search::{
        AgentSearchHit, AgentSearchQuery, AgentSearchRead, SearchRefusal, SearchScopeSelector,
    },
    foundation::identity::{ConversationId, WorkspaceId},
    foundation::name_canonicalization::is_ecmascript_trim_whitespace,
    host_runtime::runtime_credential::RuntimePrincipal,
};

use crate::conversation_messaging::messaging::agent_capability_wire::capability_denied;
use crate::conversation_messaging::messaging::author_wire::author_dto;
use crate::foundation::http_error::{
    coded_error as coded, json_error as error, logged_500 as internal,
};
use crate::foundation::wire_admission::ValidatedJson;
use crate::foundation::wire_coerce::CoercedNumber;
use crate::foundation::wire_patch::RequiredPatchField;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

pub const AGENT_SEARCH_MESSAGES_PATH: &str = "/internal/agents/{aid}/search-messages";

pub struct AgentSearchState<R> {
    pub reads: R,
}

struct Shared<R>(Arc<AgentSearchState<R>>);
impl<R> Clone for Shared<R> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn agent_search_router<R>(state: AgentSearchState<R>) -> Router
where
    R: AgentSearchRead,
{
    Router::new()
        .route(AGENT_SEARCH_MESSAGES_PATH, post(search::<R>))
        .with_state(Shared(Arc::new(state)))
}

/// `AgentSearchMessagesInputSchema`, `.strict()`. `deny_unknown_fields` fails the
/// retired `scope`/`channel_id` keys loudly. `q` and `limit` are validated in the
/// domain conversion; the selectors are the at-most-one semantic check.
#[derive(Deserialize, garde::Validate)]
#[serde(deny_unknown_fields)]
struct SearchBody {
    #[garde(skip)]
    q: String,
    #[garde(skip)]
    #[serde(default)]
    current_workspace: RequiredPatchField<bool>,
    #[garde(skip)]
    #[serde(default)]
    workspace_id: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    conversation_id: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    include_system: RequiredPatchField<bool>,
    #[garde(skip)]
    #[serde(default)]
    limit: CoercedNumber,
}

async fn search<R>(
    State(state): State<Shared<R>>,
    Path(aid): Path<String>,
    principal: Option<Extension<RuntimePrincipal>>,
    request: Request,
) -> Response
where
    R: AgentSearchRead,
{
    // IDENTITY FIRST (#25): bind `{aid}` before decoding the body.
    let principal = match bind_runtime_actor(principal.as_ref().map(|e| &e.0), &aid) {
        Ok(principal) => principal,
        Err(refusal) => return refusal.into_response(),
    };
    let agent = principal.agent().clone();
    let body = match ValidatedJson::<SearchBody>::from_request(request, &()).await {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };

    // `q`: Zod `z.string().trim().min(1).max(200)` — trim THEN gate 1..=200 UTF-16
    // units on the TRIMMED value. The trim MUST be the ECMAScript set (shared
    // `is_ecmascript_trim_whitespace` authority), not Rust `str::trim()` whose
    // Unicode-White_Space set differs (e.g. it strips U+0085 NEL that ECMAScript
    // keeps) — using `str::trim()` would both mis-count the length gate and search a
    // different term than TS.
    let q = body
        .q
        .trim_matches(is_ecmascript_trim_whitespace)
        .to_owned();
    let q_units = q.encode_utf16().count();
    if !(1..=200).contains(&q_units) {
        return invalid_input();
    }
    // include_system defaults true; limit 1..=100, default 50.
    let include_system = body.include_system.into_option().unwrap_or(true);
    if body.limit.is_invalid() {
        return invalid_input();
    }
    let limit = match body.limit.value() {
        None => 50,
        Some(value) if (1..=100).contains(&value) => value,
        Some(_) => return invalid_input(),
    };
    // current_workspace is `true`-or-absent; an explicit `false` is a bad value.
    let current_workspace = match body.current_workspace.into_option() {
        None => false,
        Some(true) => true,
        Some(false) => return invalid_input(),
    };
    let workspace_id = match body.workspace_id.into_option() {
        None => None,
        Some(raw) if raw.is_empty() => return invalid_input(),
        Some(raw) => Some(raw),
    };
    let conversation_id = match body.conversation_id.into_option() {
        None => None,
        Some(raw) if raw.is_empty() => return invalid_input(),
        Some(raw) => Some(raw),
    };
    // At most one selector.
    let selector_count = usize::from(current_workspace)
        + usize::from(workspace_id.is_some())
        + usize::from(conversation_id.is_some());
    if selector_count > 1 {
        return invalid_input();
    }

    let query = AgentSearchQuery {
        text: q.clone(),
        include_system,
        limit,
    };
    let state = &state.0;

    // The raw scope selector — the read authority resolves + authorizes it in one
    // snapshot; the route resolves nothing itself.
    let source = principal.source().cloned();
    let selector = if let Some(raw) = conversation_id {
        let Ok(conversation) = ConversationId::parse(raw) else {
            return invalid_input();
        };
        SearchScopeSelector::Conversation(conversation)
    } else if let Some(raw) = workspace_id {
        let Ok(workspace) = WorkspaceId::parse(raw) else {
            return invalid_input();
        };
        SearchScopeSelector::Workspace(workspace)
    } else if current_workspace {
        SearchScopeSelector::CurrentWorkspace
    } else {
        SearchScopeSelector::Current
    };

    let result = match state
        .reads
        .search(&agent, source.as_ref(), &selector, &query)
        .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(refusal)) => return search_refusal(refusal),
        Err(cause) => return internal(cause, "search read failed"),
    };

    let body = SearchResponse {
        query: q,
        scope: result.scope.scope.as_str(),
        workspace_id: result.scope.echo_workspace,
        conversation_id: result.scope.echo_conversation,
        include_system,
        limit,
        hits: result.hits.into_iter().map(hit_dto).collect(),
    };
    (StatusCode::OK, Json(body)).into_response()
}

/// Map the in-snapshot search refusal to the reference wire.
fn search_refusal(refusal: SearchRefusal) -> Response {
    match refusal {
        SearchRefusal::ScopeRequired => search_scope_required(),
        SearchRefusal::WorkspaceScopeRequired => workspace_scope_required(),
        SearchRefusal::SourceNotFound => error(StatusCode::NOT_FOUND, "conversation not found"),
        SearchRefusal::WorkspaceForbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        SearchRefusal::ConversationNotFound => coded(
            StatusCode::NOT_FOUND,
            "conversation not found",
            "conversation_not_found",
        ),
        SearchRefusal::NotConversationMember => coded(
            StatusCode::FORBIDDEN,
            "agent is not a member of this conversation",
            "not_conversation_member",
        ),
        SearchRefusal::CapabilityDenied => capability_denied("read_messages"),
    }
}

fn hit_dto(hit: AgentSearchHit) -> serde_json::Value {
    let coords = &hit.coordinates;
    serde_json::json!({
        "message_ref": {
            "id": hit.message_id,
            "workspace_id": coords.workspace_id.as_ref().map(|w| w.as_str()),
            "conversation_id": hit.conversation_id,
            "conversation_placement_kind": coords.placement_kind.as_str(),
            "conversation_roster_policy": coords.roster_policy.as_str(),
            "channel_id": coords.channel_id.as_str(),
            "seq": hit.seq,
            "created_at": hit.created_at,
        },
        "sender": hit.author.as_ref().map(|author| sender_ref(&author_dto(author))),
        "excerpt": hit.excerpt.excerpt,
        "match_offset": hit.excerpt.match_offset,
    })
}

/// Project the full author wire JSON down to the `MessageSenderRef` shape (drop the
/// renderer-only avatar/runtime fields; `status`/`kind`/`id`/`name`/`display_name`
/// verbatim). `null` in (system) ⇒ handled by the caller (`None`).
fn sender_ref(author: &serde_json::Value) -> serde_json::Value {
    let status = author
        .get("status")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if status == serde_json::Value::String("unresolved".to_owned()) {
        serde_json::json!({
            "status": "unresolved",
            "kind": author.get("kind").cloned().unwrap_or(serde_json::Value::Null),
            "id": serde_json::Value::Null,
            "name": serde_json::Value::Null,
            "display_name": serde_json::Value::Null,
        })
    } else {
        serde_json::json!({
            "status": status,
            "kind": author.get("kind").cloned().unwrap_or(serde_json::Value::Null),
            "id": author.get("id").cloned().unwrap_or(serde_json::Value::Null),
            "name": author.get("name").cloned().unwrap_or(serde_json::Value::Null),
            "display_name": author.get("display_name").cloned().unwrap_or(serde_json::Value::Null),
        })
    }
}

/// `AgentSearchMessagesDto`.
#[derive(Serialize)]
struct SearchResponse {
    query: String,
    scope: &'static str,
    workspace_id: Option<String>,
    conversation_id: Option<String>,
    include_system: bool,
    limit: i64,
    hits: Vec<serde_json::Value>,
}

fn invalid_input() -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({ "error": "invalid input" })),
    )
        .into_response()
}

fn workspace_scope_required() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": "this conversation has no workspace, so a workspace-scoped query must name one: pass workspace_id",
            "code": "workspace_scope_required"
        })),
    )
        .into_response()
}

fn search_scope_required() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": "agent-scoped message search must name conversation_id or workspace_id",
            "code": "search_scope_required"
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request as HttpRequest;
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::agent_search::AgentSearchScope;
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::agent_search::{
        AgentSearchResult, ResolvedSearchScope,
    };
    use silverhand_backend_core::foundation::identity::{
        AgentId, ConversationId, DaemonInstanceId,
    };
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use silverhand_backend_core::host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeGeneration, RuntimeScope,
    };
    use std::sync::Mutex;
    use tower::ServiceExt;

    const AGENT: &str = "agent-1";

    fn principal() -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::ConversationSlot {
                agent: AgentId::parse(AGENT).unwrap(),
                source: ConversationId::parse("src").unwrap(),
            },
            RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        )
    }

    // Records the search TERM that reached the authority; returns no hits.
    #[derive(Clone)]
    struct RecordingRead(std::sync::Arc<Mutex<Option<String>>>);
    impl AgentSearchRead for RecordingRead {
        async fn search(
            &self,
            _agent: &AgentId,
            _source: Option<&ConversationId>,
            _selector: &SearchScopeSelector,
            query: &AgentSearchQuery,
        ) -> Result<Result<AgentSearchResult, SearchRefusal>, PersistenceFailure> {
            *self.0.lock().unwrap() = Some(query.text.clone());
            Ok(Ok(AgentSearchResult {
                scope: ResolvedSearchScope {
                    scope: AgentSearchScope::Current,
                    echo_workspace: None,
                    echo_conversation: Some("src".to_owned()),
                },
                hits: vec![],
            }))
        }
    }

    async fn search_q(
        aid: &str,
        raw_q_json: &str,
    ) -> (StatusCode, serde_json::Value, Option<String>) {
        let captured = std::sync::Arc::new(Mutex::new(None));
        let router = agent_search_router(AgentSearchState {
            reads: RecordingRead(std::sync::Arc::clone(&captured)),
        })
        .layer(Extension(principal()));
        let response = router
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri(format!("/internal/agents/{aid}/search-messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!("{{\"q\":{raw_q_json}}}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        let term = captured.lock().unwrap().clone();
        (status, json, term)
    }

    #[tokio::test]
    async fn q_trim_uses_ecmascript_whitespace_not_rust_trim() {
        // U+0085 (NEL): Rust White_Space but NOT ECMAScript trim ws → KEPT.
        let (status, body, term) = search_q(AGENT, "\"\\u0085hi\\u0085\"").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(term.as_deref(), Some("\u{0085}hi\u{0085}"));
        assert_eq!(body["query"], "\u{0085}hi\u{0085}");
        // U+FEFF: ECMAScript trim ws → trimmed.
        let (status, _b, term) = search_q(AGENT, "\"\\uFEFFhi\"").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(term.as_deref(), Some("hi"));
        // lone NEL is non-empty after ECMAScript trim → accepted.
        let (status, _b, term) = search_q(AGENT, "\"\\u0085\"").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(term.as_deref(), Some("\u{0085}"));
        // lone BOM trims empty → 422.
        let (status, _b, _t) = search_q(AGENT, "\"\\uFEFF\"").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn identity_binds_before_body_extraction() {
        // #25: mismatched aid + malformed body → 401 before the body decodes, read
        // authority zero-reach.
        let captured = std::sync::Arc::new(Mutex::new(None));
        let router = agent_search_router(AgentSearchState {
            reads: RecordingRead(std::sync::Arc::clone(&captured)),
        })
        .layer(Extension(principal()));
        let response = router
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/internal/agents/agent-OTHER/search-messages")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            captured.lock().unwrap().is_none(),
            "read authority zero-reach on identity refusal"
        );
    }
}
