//! Typed `cli.invoke` observation for authenticated internal-agent routes.

use std::{
    panic::{AssertUnwindSafe, resume_unwind},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Instant,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{MatchedPath, Request, rejection::JsonRejection},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::Response,
};
use futures_util::FutureExt;
use silverhand_backend_core::{
    foundation::cli_invoke_audit::{
        CliInvoke, CliInvokeAuditSink, CliInvokeErrorCode, CliInvokeErrorKind, CliInvokeIssue,
        CliInvokeIssueCode, CliInvokeIssues, CliInvokeMethod, CliInvokeRoute, CliRefusalCode,
        CliStaleResolutionReason,
    },
    foundation::cli_preflight_audit::NonNegativeFiniteF64,
    foundation::identity::ConversationId,
    host_runtime::runtime_credential::RuntimePrincipal,
};
use tracing::Instrument;

const CLI_TRACE_PATH: &str = "/internal/agents/{aid}/cli-trace";
const AVATAR_PATH: &str = "/internal/agents/{aid}/avatar";
const REACTION_PATH: &str = "/internal/agents/{aid}/react-message";
const ATTACHMENTS_PATH: &str = "/internal/agents/{aid}/attachments";
const ATTACHMENT_PATH: &str = "/internal/agents/{aid}/attachments/{attachment_id}";
const ATTACHMENT_LOCATE_PATH: &str = "/internal/agents/{aid}/attachments/{attachment_id}/locate";
const ATTACHMENT_DOWNLOAD_PATH: &str =
    "/internal/agents/{aid}/attachments/{attachment_id}/download";

/// Exact method/path perimeter currently composed under runtime credentials. Some faces
/// intentionally have no CLI command identity, but they still produce an authenticated
/// invocation record with the principal preserved.
pub const CLI_INVOKE_PRODUCTION_PERIMETER: &[(CliInvokeMethod, &str)] = &[
    (CliInvokeMethod::Post, "/internal/agents/{aid}/send-message"),
    (CliInvokeMethod::Post, REACTION_PATH),
    (CliInvokeMethod::Delete, REACTION_PATH),
    (CliInvokeMethod::Post, ATTACHMENTS_PATH),
    (CliInvokeMethod::Patch, ATTACHMENT_PATH),
    (CliInvokeMethod::Get, ATTACHMENT_LOCATE_PATH),
    (CliInvokeMethod::Get, ATTACHMENT_DOWNLOAD_PATH),
    (CliInvokeMethod::Get, "/internal/agents/{aid}/tasks"),
    (
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/stale-message-resolution",
    ),
    (CliInvokeMethod::Get, "/internal/agents/{aid}/profile"),
    (CliInvokeMethod::Post, "/internal/agents/{aid}/profile"),
    (CliInvokeMethod::Post, AVATAR_PATH),
    (CliInvokeMethod::Delete, AVATAR_PATH),
    (CliInvokeMethod::Get, "/internal/agents/{aid}/peer-profile"),
    (CliInvokeMethod::Get, "/internal/agents/{aid}/profile/mcp"),
    (CliInvokeMethod::Post, "/internal/agents/{aid}/profile/mcp"),
    (
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/profile/mcp/probe",
    ),
    (CliInvokeMethod::Post, CLI_TRACE_PATH),
    (CliInvokeMethod::Post, "/internal/agents/{aid}/members"),
    (CliInvokeMethod::Post, "/internal/agents/{aid}/tasks/create"),
    (CliInvokeMethod::Post, "/internal/agents/{aid}/tasks/claim"),
    (
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/tasks/unclaim",
    ),
    (
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/tasks/update-status",
    ),
    (CliInvokeMethod::Post, "/internal/agents/{aid}/tasks/edit"),
    (CliInvokeMethod::Get, "/internal/agents/{aid}/tasks/detail"),
    (CliInvokeMethod::Post, "/internal/agents/{aid}/schedules"),
    (
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/schedules/list",
    ),
    (
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/schedules/cancel",
    ),
    (
        CliInvokeMethod::Get,
        "/internal/agents/{aid}/list-workspaces",
    ),
    (CliInvokeMethod::Get, "/internal/agents/{aid}/list-channels"),
    (
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/follow-thread",
    ),
    (
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/unfollow-thread",
    ),
    (
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/search-messages",
    ),
    (CliInvokeMethod::Post, "/internal/agents/{aid}/read-history"),
    (
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/create-channel",
    ),
];

#[derive(Clone, Copy)]
struct RouteDescriptor {
    method: CliInvokeMethod,
    path: &'static str,
    route: CliInvokeRoute,
    query_allowlist: &'static [&'static str],
}

const TASK_QUERY_KEYS: &[&str] = &[
    "assignee_id",
    "conversation_id",
    "limit",
    "status",
    "task_number",
    "workspace_id",
];
const PEER_PROFILE_QUERY_KEYS: &[&str] = &["handle", "workspace_id"];

const ROUTES: &[RouteDescriptor] = &[
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/send-message",
        "message",
        "send",
        "send-message",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        REACTION_PATH,
        "message",
        "react",
        "react-message",
        &[],
    ),
    route(
        CliInvokeMethod::Delete,
        REACTION_PATH,
        "message",
        "react",
        "react-message",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        ATTACHMENTS_PATH,
        "file",
        "upload",
        "attachments",
        &[],
    ),
    route(
        CliInvokeMethod::Patch,
        ATTACHMENT_PATH,
        "file",
        "update",
        "attachments/*",
        &[],
    ),
    route(
        CliInvokeMethod::Get,
        ATTACHMENT_LOCATE_PATH,
        "file",
        "view",
        "attachments/*/locate",
        &[],
    ),
    route(
        CliInvokeMethod::Get,
        ATTACHMENT_DOWNLOAD_PATH,
        "file",
        "view",
        "attachments/*/download",
        &[],
    ),
    route(
        CliInvokeMethod::Get,
        "/internal/agents/{aid}/tasks",
        "task",
        "list",
        "tasks",
        TASK_QUERY_KEYS,
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/stale-message-resolution",
        "message",
        "stale-resolve",
        "stale-message-resolution",
        &[],
    ),
    route(
        CliInvokeMethod::Get,
        "/internal/agents/{aid}/profile",
        "profile",
        "get",
        "profile",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/profile",
        "profile",
        "update",
        "profile",
        &[],
    ),
    route(
        CliInvokeMethod::Get,
        "/internal/agents/{aid}/peer-profile",
        "profile",
        "get",
        "peer-profile",
        PEER_PROFILE_QUERY_KEYS,
    ),
    route(
        CliInvokeMethod::Get,
        "/internal/agents/{aid}/profile/mcp",
        "profile",
        "mcp get",
        "profile/mcp",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/profile/mcp",
        "profile",
        "mcp set",
        "profile/mcp",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/profile/mcp/probe",
        "profile",
        "mcp probe",
        "profile/mcp/probe",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/members",
        "member",
        "add",
        "members",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/tasks/create",
        "task",
        "create",
        "tasks/create",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/tasks/claim",
        "task",
        "claim",
        "tasks/claim",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/tasks/unclaim",
        "task",
        "unclaim",
        "tasks/unclaim",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/tasks/update-status",
        "task",
        "status",
        "tasks/update-status",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/tasks/edit",
        "task",
        "edit",
        "tasks/edit",
        &[],
    ),
    route(
        CliInvokeMethod::Get,
        "/internal/agents/{aid}/tasks/detail",
        "task",
        "detail",
        "tasks/detail",
        TASK_QUERY_KEYS,
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/schedules",
        "schedule",
        "create",
        "schedules",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/schedules/list",
        "schedule",
        "list",
        "schedules/list",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/schedules/cancel",
        "schedule",
        "cancel",
        "schedules/cancel",
        &[],
    ),
    route(
        CliInvokeMethod::Get,
        "/internal/agents/{aid}/list-workspaces",
        "workspace",
        "list",
        "list-workspaces",
        &[],
    ),
    route(
        CliInvokeMethod::Get,
        "/internal/agents/{aid}/list-channels",
        "channel",
        "list",
        "list-channels",
        &["workspace_id"],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/follow-thread",
        "thread",
        "follow",
        "follow-thread",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/unfollow-thread",
        "thread",
        "unfollow",
        "unfollow-thread",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/search-messages",
        "message",
        "search",
        "search-messages",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/read-history",
        "message",
        "history",
        "read-history",
        &[],
    ),
    route(
        CliInvokeMethod::Post,
        "/internal/agents/{aid}/create-channel",
        "channel",
        "create",
        "create-channel",
        &[],
    ),
];

const fn route(
    method: CliInvokeMethod,
    path: &'static str,
    noun: &'static str,
    verb: &'static str,
    route_suffix: &'static str,
    query_allowlist: &'static [&'static str],
) -> RouteDescriptor {
    RouteDescriptor {
        method,
        path,
        route: CliInvokeRoute {
            noun,
            verb,
            route_suffix,
        },
        query_allowlist,
    }
}

#[derive(Clone)]
struct MiddlewareState<S> {
    sink: S,
    next_request_id: Arc<AtomicU64>,
}

/// Installs observation on already-declared internal-agent routes. Composition must apply
/// runtime authentication after this function so the auth layer stays outside this one.
pub fn audit_cli_invocations<S>(router: Router, sink: S) -> Router
where
    S: CliInvokeAuditSink,
{
    router.route_layer(middleware::from_fn_with_state(
        MiddlewareState {
            sink,
            next_request_id: Arc::new(AtomicU64::new(1)),
        },
        observe::<S>,
    ))
}

async fn observe<S>(
    axum::extract::State(state): axum::extract::State<MiddlewareState<S>>,
    mut request: Request,
    next: Next,
) -> Response
where
    S: CliInvokeAuditSink,
{
    let matched_path = request.extensions().get::<MatchedPath>().cloned();
    if matched_path.as_ref().map(MatchedPath::as_str) == Some(CLI_TRACE_PATH) {
        return next.run(request).await;
    }
    let Some(principal) = request.extensions().get::<RuntimePrincipal>().cloned() else {
        // This layer is intentionally inside runtime auth. Absence means composition is
        // wrong; do not fabricate an identity or an unauthenticated audit line.
        return next.run(request).await;
    };
    let method = closed_method(request.method());
    let descriptor = matched_path
        .as_ref()
        .and_then(|path| lookup(method, path.as_str()));
    let query_keys = descriptor
        .map(|route| allowlisted_query_keys(request.uri().query(), route.query_allowlist))
        .unwrap_or_default();
    let request_id = format!(
        "rust-{}-{}",
        std::process::id(),
        state.next_request_id.fetch_add(1, Ordering::Relaxed)
    );
    let context = CliInvokeRequestContext::default();
    request.extensions_mut().insert(context.clone());
    let started = Instant::now();

    // A span emits no event by itself. ERROR keeps its generated identity
    // available to failure logs under the production ERROR-only default filter.
    let span = tracing::error_span!("cli_request", request_id = request_id.as_str());
    match AssertUnwindSafe(next.run(request).instrument(span))
        .catch_unwind()
        .await
    {
        Ok(response) => {
            let status = response.status();
            let terminal = response
                .extensions()
                .get::<CliInvokeResponseMeta>()
                .cloned();
            let (response, refusal_code) = refusal_code(response).await;
            record(
                &state.sink,
                request_id,
                principal,
                descriptor,
                method,
                query_keys,
                status,
                started,
                terminal,
                refusal_code,
                &context,
                None,
            );
            response
        }
        Err(payload) => {
            record(
                &state.sink,
                request_id,
                principal,
                descriptor,
                method,
                query_keys,
                StatusCode::INTERNAL_SERVER_ERROR,
                started,
                None,
                None,
                &context,
                Some(CliInvokeErrorKind::Panic),
            );
            resume_unwind(payload)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record<S: CliInvokeAuditSink>(
    sink: &S,
    request_id: String,
    principal: RuntimePrincipal,
    descriptor: Option<RouteDescriptor>,
    method: CliInvokeMethod,
    query_keys: Vec<&'static str>,
    status: StatusCode,
    started: Instant,
    terminal: Option<CliInvokeResponseMeta>,
    refusal_code: Option<CliRefusalCode>,
    context: &CliInvokeRequestContext,
    error_kind: Option<CliInvokeErrorKind>,
) {
    let elapsed = NonNegativeFiniteF64::new(started.elapsed().as_secs_f64() * 1_000.0)
        .unwrap_or(NonNegativeFiniteF64::ZERO);
    let typed_code = terminal.as_ref().and_then(|meta| meta.error_code);
    let error_issues = terminal.map(|meta| meta.error_issues).unwrap_or_default();
    let invoke = CliInvoke {
        request_id,
        agent_id: principal.agent().clone(),
        route: descriptor.map(|descriptor| descriptor.route),
        method,
        query_keys,
        http_status: status.as_u16(),
        duration_ms: elapsed,
        error_code: typed_code.or_else(|| fallback_error_code(status)),
        refusal_code,
        error_issues: CliInvokeIssues::new(error_issues),
        conversation_id: context.conversation(),
        stale_resolution_reason: context.stale_reason(),
        error_kind,
    };
    sink.record(invoke);
}

/// Error bodies are small buffered JSON; anything else is passed through unread.
const REFUSAL_BODY_LIMIT: usize = 16 * 1024;

/// Read the machine `code` of a JSON error body, then rebuild the response unchanged.
async fn refusal_code(response: Response) -> (Response, Option<CliRefusalCode>) {
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let bounded = http_body::Body::size_hint(response.body())
        .exact()
        .is_some_and(|len| len <= REFUSAL_BODY_LIMIT as u64);
    if response.status().is_success() || !is_json || !bounded {
        return (response, None);
    }
    let (parts, body) = response.into_parts();
    // Infallible for a buffered body of exact, bounded length.
    let bytes = to_bytes(body, REFUSAL_BODY_LIMIT).await.unwrap_or_default();
    let code = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .get("code")
                .and_then(serde_json::Value::as_str)
                .and_then(CliRefusalCode::parse)
        });
    (Response::from_parts(parts, Body::from(bytes)), code)
}

fn lookup(method: CliInvokeMethod, path: &str) -> Option<RouteDescriptor> {
    ROUTES
        .iter()
        .copied()
        .find(|route| route.method == method && route.path == path)
}

fn closed_method(method: &Method) -> CliInvokeMethod {
    match *method {
        Method::GET => CliInvokeMethod::Get,
        Method::POST => CliInvokeMethod::Post,
        Method::PATCH => CliInvokeMethod::Patch,
        Method::DELETE => CliInvokeMethod::Delete,
        Method::PUT => CliInvokeMethod::Put,
        Method::HEAD => CliInvokeMethod::Head,
        Method::OPTIONS => CliInvokeMethod::Options,
        _ => CliInvokeMethod::Other,
    }
}

fn allowlisted_query_keys(
    raw_query: Option<&str>,
    allowlist: &'static [&'static str],
) -> Vec<&'static str> {
    let Some(raw_query) = raw_query else {
        return Vec::new();
    };
    let mut present = Vec::new();
    for pair in raw_query.split('&') {
        let encoded_key = pair.split_once('=').map_or(pair, |(key, _)| key);
        let decoded = form_urlencoded::parse(encoded_key.as_bytes())
            .next()
            .map(|(key, _)| key);
        if let Some(key) = decoded {
            for allowed in allowlist {
                if key == *allowed && !present.contains(allowed) {
                    present.push(*allowed);
                }
            }
        }
    }
    present.sort_unstable();
    present
}

fn fallback_error_code(status: StatusCode) -> Option<CliInvokeErrorCode> {
    match status {
        StatusCode::BAD_REQUEST
        | StatusCode::UNSUPPORTED_MEDIA_TYPE
        | StatusCode::UNPROCESSABLE_ENTITY => Some(CliInvokeErrorCode::InvalidInput),
        StatusCode::UNAUTHORIZED => Some(CliInvokeErrorCode::Unauthorized),
        StatusCode::FORBIDDEN => Some(CliInvokeErrorCode::Forbidden),
        StatusCode::NOT_FOUND => Some(CliInvokeErrorCode::NotFound),
        StatusCode::CONFLICT => Some(CliInvokeErrorCode::Conflict),
        StatusCode::PAYLOAD_TOO_LARGE => Some(CliInvokeErrorCode::PayloadTooLarge),
        status if status.is_server_error() => Some(CliInvokeErrorCode::InternalError),
        _ => None,
    }
}

const STALE_UNSET: u8 = 0;
const STALE_UNAFFECTED: u8 = 1;
const STALE_REVISED: u8 = 2;
const STALE_OBSOLETE: u8 = 3;
const STALE_FORCE_SEND: u8 = 4;
const STALE_UNATTRIBUTED: u8 = 5;

/// One-way request metadata. A later layer cannot replace the first authorized decision.
#[derive(Clone, Default)]
pub(crate) struct CliInvokeRequestContext(Arc<RequestFacts>);

#[derive(Default)]
struct RequestFacts {
    stale: AtomicU8,
    conversation: OnceLock<ConversationId>,
}

impl CliInvokeRequestContext {
    /// Record the conversation the request resolved to act on (first write wins).
    pub(crate) fn set_conversation(&self, conversation: &ConversationId) {
        let _ = self.0.conversation.set(conversation.clone());
    }

    fn conversation(&self) -> Option<ConversationId> {
        self.0.conversation.get().cloned()
    }

    pub(crate) fn set_stale_reason(&self, reason: CliStaleResolutionReason) {
        let encoded = match reason {
            CliStaleResolutionReason::Unaffected => STALE_UNAFFECTED,
            CliStaleResolutionReason::Revised => STALE_REVISED,
            CliStaleResolutionReason::Obsolete => STALE_OBSOLETE,
            CliStaleResolutionReason::ForceSend => STALE_FORCE_SEND,
            CliStaleResolutionReason::Unattributed => STALE_UNATTRIBUTED,
        };
        let _ = self.0.stale.compare_exchange(
            STALE_UNSET,
            encoded,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    pub(crate) fn stale_reason(&self) -> Option<CliStaleResolutionReason> {
        match self.0.stale.load(Ordering::Acquire) {
            STALE_UNAFFECTED => Some(CliStaleResolutionReason::Unaffected),
            STALE_REVISED => Some(CliStaleResolutionReason::Revised),
            STALE_OBSOLETE => Some(CliStaleResolutionReason::Obsolete),
            STALE_FORCE_SEND => Some(CliStaleResolutionReason::ForceSend),
            STALE_UNATTRIBUTED => Some(CliStaleResolutionReason::Unattributed),
            _ => None,
        }
    }
}

/// Terminal classification carried in-process on a response, never serialized to HTTP.
#[derive(Clone)]
pub(crate) struct CliInvokeResponseMeta {
    error_code: Option<CliInvokeErrorCode>,
    error_issues: Vec<CliInvokeIssue>,
}

impl CliInvokeResponseMeta {
    pub(crate) fn invalid_json(rejection: &JsonRejection) -> Self {
        let issue = match rejection {
            JsonRejection::MissingJsonContentType(_) => {
                CliInvokeIssue::body(CliInvokeIssueCode::UnsupportedMediaType)
            }
            JsonRejection::JsonSyntaxError(_) => {
                CliInvokeIssue::body(CliInvokeIssueCode::MalformedJson)
            }
            JsonRejection::JsonDataError(_) | JsonRejection::BytesRejection(_) => {
                CliInvokeIssue::body(CliInvokeIssueCode::InvalidStructure)
            }
            _ => CliInvokeIssue::body(CliInvokeIssueCode::InvalidStructure),
        };
        Self {
            error_code: Some(CliInvokeErrorCode::InvalidInput),
            error_issues: vec![issue],
        }
    }

    pub(crate) fn invalid_value() -> Self {
        Self {
            error_code: Some(CliInvokeErrorCode::InvalidInput),
            error_issues: vec![CliInvokeIssue::body(CliInvokeIssueCode::InvalidValue)],
        }
    }
}

pub(crate) fn attach_response_meta(
    mut response: Response,
    metadata: CliInvokeResponseMeta,
) -> Response {
    response.extensions_mut().insert(metadata);
    response
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::{
        body::{Body, to_bytes},
        extract::Extension,
        http::{Request as HttpRequest, header},
        response::IntoResponse,
        routing::{get, post},
    };
    use garde::Validate;
    use serde::Deserialize;
    use silverhand_backend_core::{
        foundation::identity::{AgentId, DaemonInstanceId},
        host_runtime::runtime_credential::{
            RuntimeCustody, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
        },
    };
    use tower::ServiceExt;

    use crate::foundation::wire_admission::ValidatedJson;

    use super::*;

    #[derive(Clone, Default)]
    struct RecordingSink(Arc<Mutex<Vec<CliInvoke>>>);

    impl CliInvokeAuditSink for RecordingSink {
        fn record(&self, invoke: CliInvoke) {
            self.0.lock().unwrap().push(invoke);
        }
    }

    impl RecordingSink {
        fn recorded(&self) -> Vec<CliInvoke> {
            self.0.lock().unwrap().clone()
        }
    }

    fn principal() -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("a1").unwrap(),
            },
            RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        )
    }

    fn request(method: &str, uri: &str, body: &'static str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .extension(principal())
            .body(Body::from(body))
            .unwrap()
    }

    async fn response_parts(response: Response) -> (StatusCode, Vec<u8>) {
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, bytes.to_vec())
    }

    #[derive(Deserialize, Validate)]
    #[serde(deny_unknown_fields)]
    struct Probe {
        #[garde(length(max = 3))]
        label: String,
    }

    async fn probe(ValidatedJson(body): ValidatedJson<Probe>) -> StatusCode {
        let _ = body.label;
        StatusCode::NO_CONTENT
    }

    fn probe_router() -> Router {
        Router::new().route("/internal/agents/{aid}/profile/mcp/probe", post(probe))
    }

    #[test]
    fn current_cli_routes_are_a_closed_unique_method_path_set() {
        for (index, left) in ROUTES.iter().enumerate() {
            assert!(
                !ROUTES[index + 1..]
                    .iter()
                    .any(|right| left.method == right.method && left.path == right.path),
                "duplicate CLI invoke route: {} {}",
                left.method.as_str(),
                left.path
            );
        }
        assert_eq!(ROUTES.len(), 32);
        let mut accounted: Vec<_> = ROUTES
            .iter()
            .map(|route| (route.method, route.path))
            .collect();
        accounted.extend([
            (CliInvokeMethod::Post, AVATAR_PATH),
            (CliInvokeMethod::Delete, AVATAR_PATH),
            (CliInvokeMethod::Post, CLI_TRACE_PATH),
        ]);
        accounted.sort_unstable_by_key(|(method, path)| (method.as_str(), *path));
        let mut perimeter = CLI_INVOKE_PRODUCTION_PERIMETER.to_vec();
        perimeter.sort_unstable_by_key(|(method, path)| (method.as_str(), *path));
        assert_eq!(
            accounted, perimeter,
            "every production face is mapped, intentionally unmapped, or trace-excluded"
        );
    }

    #[test]
    fn rust_audit_identity_matches_the_cli_route_contract_in_both_directions() {
        let reference: Vec<serde_json::Value> = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/shared/src/cli-route-contract.json"
        )))
        .unwrap();
        let expected: std::collections::BTreeSet<_> = reference
            .iter()
            .map(|r| {
                (
                    r["method"].as_str().unwrap(),
                    r["suffix"].as_str().unwrap(),
                    r["noun"].as_str().unwrap(),
                    r["verb"].as_str().unwrap(),
                )
            })
            .collect();
        let actual: std::collections::BTreeSet<_> = ROUTES
            .iter()
            .map(|r| {
                (
                    r.method.as_str(),
                    r.route.route_suffix,
                    r.route.noun,
                    r.route.verb,
                )
            })
            .collect();
        assert_eq!(reference.len(), expected.len(), "duplicate contract entry");
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn channel_list_records_only_the_allowed_workspace_key() {
        let sink = RecordingSink::default();
        let app = audit_cli_invocations(
            Router::new().route(
                "/internal/agents/{aid}/list-channels",
                get(|| async { StatusCode::BAD_REQUEST }),
            ),
            sink.clone(),
        );
        for query in [
            "",
            "?workspace_id=private-workspace&unknown=private-value&workspace_id=second",
        ] {
            app.clone()
                .oneshot(request(
                    "GET",
                    &format!("/internal/agents/a1/list-channels{query}"),
                    "",
                ))
                .await
                .unwrap();
        }
        let records = sink.recorded();
        assert!(records[0].query_keys.is_empty());
        assert_eq!(records[1].query_keys, vec!["workspace_id"]);
        assert_eq!(records[1].route.unwrap().noun, "channel");
    }

    #[tokio::test]
    async fn a_coded_error_body_records_its_code_and_reaches_the_caller_unchanged() {
        let route = || {
            Router::new().route(
                "/internal/agents/{aid}/list-channels",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        axum::Json(serde_json::json!({
                            "error": "target names no stream you can use",
                            "code": "target_unresolved",
                        })),
                    )
                }),
            )
        };
        let build = || request("GET", "/internal/agents/a1/list-channels", "");
        let baseline = response_parts(route().oneshot(build()).await.unwrap()).await;
        let sink = RecordingSink::default();
        let observed = response_parts(
            audit_cli_invocations(route(), sink.clone())
                .oneshot(build())
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(observed, baseline);
        let records = sink.recorded();
        assert_eq!(
            records[0].refusal_code.as_ref().map(CliRefusalCode::as_str),
            Some("target_unresolved")
        );
        assert_eq!(records[0].conversation_id, None);
    }

    #[tokio::test]
    async fn a_route_reported_conversation_is_recorded() {
        let sink = RecordingSink::default();
        let app = audit_cli_invocations(
            Router::new().route(
                "/internal/agents/{aid}/list-channels",
                get(
                    |Extension(context): Extension<CliInvokeRequestContext>| async move {
                        context.set_conversation(&ConversationId::parse("conv-1").unwrap());
                        StatusCode::OK
                    },
                ),
            ),
            sink.clone(),
        );
        app.oneshot(request("GET", "/internal/agents/a1/list-channels", ""))
            .await
            .unwrap();
        let records = sink.recorded();
        assert_eq!(
            records[0].conversation_id,
            Some(ConversationId::parse("conv-1").unwrap())
        );
        assert_eq!(records[0].refusal_code, None);
    }

    #[tokio::test]
    async fn validated_json_keeps_native_wire_while_typed_metadata_is_observed_once() {
        struct Case {
            body: &'static str,
            content_type: bool,
            status: StatusCode,
            issue: CliInvokeIssueCode,
        }
        let cases = [
            Case {
                body: "not json",
                content_type: true,
                status: StatusCode::BAD_REQUEST,
                issue: CliInvokeIssueCode::MalformedJson,
            },
            Case {
                body: r#"{"label":42}"#,
                content_type: true,
                status: StatusCode::UNPROCESSABLE_ENTITY,
                issue: CliInvokeIssueCode::InvalidStructure,
            },
            Case {
                body: r#"{"label":"ok"}"#,
                content_type: false,
                status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                issue: CliInvokeIssueCode::UnsupportedMediaType,
            },
        ];

        for case in cases {
            let build = || {
                let mut builder = HttpRequest::builder()
                    .method("POST")
                    .uri("/internal/agents/a1/profile/mcp/probe")
                    .extension(principal());
                if case.content_type {
                    builder = builder.header(header::CONTENT_TYPE, "application/json");
                }
                builder.body(Body::from(case.body)).unwrap()
            };
            let baseline = response_parts(probe_router().oneshot(build()).await.unwrap()).await;
            let sink = RecordingSink::default();
            let observed = response_parts(
                audit_cli_invocations(probe_router(), sink.clone())
                    .oneshot(build())
                    .await
                    .unwrap(),
            )
            .await;

            assert_eq!(
                observed, baseline,
                "audit metadata cannot change native wire"
            );
            assert_eq!(observed.0, case.status);
            let records = sink.recorded();
            assert_eq!(records.len(), 1);
            assert_eq!(
                records[0].error_code,
                Some(CliInvokeErrorCode::InvalidInput)
            );
            assert_eq!(
                records[0].error_issues.as_slice(),
                [CliInvokeIssue::body(case.issue)]
            );
        }
    }

    #[tokio::test]
    async fn semantic_rejection_is_minimal_and_is_classified_without_validator_detail() {
        let sink = RecordingSink::default();
        let response = audit_cli_invocations(probe_router(), sink.clone())
            .oneshot(request(
                "POST",
                "/internal/agents/a1/profile/mcp/probe",
                r#"{"label":"toolong"}"#,
            ))
            .await
            .unwrap();
        let (status, body) = response_parts(response).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body, br#"{"error":"invalid input"}"#);
        let records = sink.recorded();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].error_issues.as_slice(),
            [CliInvokeIssue::body(CliInvokeIssueCode::InvalidValue)]
        );
    }

    #[tokio::test]
    async fn typed_terminal_code_precedes_the_status_fallback() {
        async fn typed_failure() -> Response {
            attach_response_meta(
                StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                CliInvokeResponseMeta {
                    error_code: Some(CliInvokeErrorCode::Conflict),
                    error_issues: Vec::new(),
                },
            )
        }

        let sink = RecordingSink::default();
        audit_cli_invocations(
            Router::new().route("/internal/agents/{aid}/profile", get(typed_failure)),
            sink.clone(),
        )
        .oneshot(request("GET", "/internal/agents/a1/profile", ""))
        .await
        .unwrap();
        let records = sink.recorded();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].http_status, 500);
        assert_eq!(records[0].error_code, Some(CliInvokeErrorCode::Conflict));
    }

    #[tokio::test]
    async fn authenticated_results_are_exactly_once_and_trace_or_pre_auth_results_are_zero() {
        let sink = RecordingSink::default();
        let router = Router::new()
            .route(
                "/internal/agents/{aid}/profile",
                get(|| async { StatusCode::OK }).post(|| async {
                    (StatusCode::INTERNAL_SERVER_ERROR, "returned failure").into_response()
                }),
            )
            .route(
                "/internal/agents/{aid}/cli-trace",
                post(|| async { StatusCode::OK }),
            );
        let app = audit_cli_invocations(router, sink.clone());

        let ok = app
            .clone()
            .oneshot(request("GET", "/internal/agents/a1/profile", ""))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let failure = app
            .clone()
            .oneshot(request("POST", "/internal/agents/a1/profile", "{}"))
            .await
            .unwrap();
        assert_eq!(failure.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let trace = app
            .clone()
            .oneshot(request("POST", "/internal/agents/a1/cli-trace", "{}"))
            .await
            .unwrap();
        assert_eq!(trace.status(), StatusCode::OK);
        let no_principal = HttpRequest::builder()
            .method("GET")
            .uri("/internal/agents/a1/profile")
            .body(Body::empty())
            .unwrap();
        let pre_auth = app.oneshot(no_principal).await.unwrap();
        assert_eq!(pre_auth.status(), StatusCode::OK);

        let records = sink.recorded();
        assert_eq!(records.len(), 2, "success and returned 500 only");
        assert_eq!(records[0].http_status, 200);
        assert_eq!(records[1].http_status, 500);
        assert_eq!(
            records[1].error_code,
            Some(CliInvokeErrorCode::InternalError)
        );
    }

    #[tokio::test]
    async fn outer_auth_401_and_auth_storage_500_are_zero_record() {
        async fn auth_gate(request: Request, next: Next) -> Response {
            match request
                .headers()
                .get("x-auth-outcome")
                .and_then(|value| value.to_str().ok())
            {
                Some("storage-failure") => {
                    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
                }
                Some("admit") => next.run(request).await,
                _ => StatusCode::UNAUTHORIZED.into_response(),
            }
        }

        let sink = RecordingSink::default();
        let app = audit_cli_invocations(
            Router::new().route(
                "/internal/agents/{aid}/profile",
                get(|| async { StatusCode::OK }),
            ),
            sink.clone(),
        )
        .route_layer(middleware::from_fn(auth_gate));
        for outcome in ["unauthorized", "storage-failure"] {
            let response = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .method("GET")
                        .uri("/internal/agents/a1/profile")
                        .header("x-auth-outcome", outcome)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::INTERNAL_SERVER_ERROR
            ));
        }
        assert!(sink.recorded().is_empty());
    }

    #[tokio::test]
    async fn panic_records_authorized_stale_fact_once_then_resumes_unwind() {
        async fn panics_after_authority(Extension(context): Extension<CliInvokeRequestContext>) {
            context.set_stale_reason(CliStaleResolutionReason::Obsolete);
            panic!("handler panic");
        }

        let sink = RecordingSink::default();
        let app = audit_cli_invocations(
            Router::new().route(
                "/internal/agents/{aid}/stale-message-resolution",
                post(panics_after_authority),
            ),
            sink.clone(),
        );
        let result = AssertUnwindSafe(app.oneshot(request(
            "POST",
            "/internal/agents/a1/stale-message-resolution",
            "{}",
        )))
        .catch_unwind()
        .await;
        assert!(result.is_err(), "middleware must resume the handler panic");
        let records = sink.recorded();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].http_status, 500);
        assert_eq!(records[0].error_kind, Some(CliInvokeErrorKind::Panic));
        assert_eq!(
            records[0].stale_resolution_reason,
            Some(CliStaleResolutionReason::Obsolete)
        );
    }

    #[tokio::test]
    async fn query_values_and_unregistered_route_identity_are_never_recorded() {
        let sink = RecordingSink::default();
        let router = Router::new()
            .route(
                "/internal/agents/{aid}/tasks",
                get(|| async { StatusCode::OK }),
            )
            .route(
                "/internal/agents/{aid}/unmapped",
                get(|| async { StatusCode::NOT_FOUND }),
            );
        let app = audit_cli_invocations(router, sink.clone());
        app.clone()
            .oneshot(request(
                "GET",
                "/internal/agents/a1/tasks?task_number=1&status=todo&secret=hunter2&status=done",
                "",
            ))
            .await
            .unwrap();
        app.oneshot(request(
            "GET",
            "/internal/agents/a1/unmapped?token=secret",
            "",
        ))
        .await
        .unwrap();

        let records = sink.recorded();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].query_keys, vec!["status", "task_number"]);
        assert_eq!(records[0].agent_id.as_str(), "a1");
        assert_eq!(records[1].agent_id.as_str(), "a1");
        assert_eq!(records[1].route, None);
        assert!(records[1].query_keys.is_empty());
    }
}
