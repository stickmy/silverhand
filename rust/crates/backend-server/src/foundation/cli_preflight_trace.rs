//! `POST /internal/agents/{aid}/cli-trace` — the CLI preflight-failure trace sink.
//!
//! ## Self-scoped, records through an injected port, no persistence of its own
//!
//! Like the C1 self-profile routes this route sits behind the self-pin credential layer
//! ([`require_runtime_credential`](crate::host_runtime::runtime_credential::require_runtime_credential)):
//! `{aid}` is bound to the caller's own agent before the handler runs, so the handler takes
//! NO path param — the subject is `credential.agent()`, never a wire value, and the source
//! conversation (present only for a ConversationSlot) is `credential.source()`. An absent
//! credential is the composition-fault safe `500`, never a `401` (that verdict is the
//! layer's).
//!
//! The body is an object admitted through the frozen
//! [`ValidatedJson`]/[`ObjectOnly`](crate::foundation::wire_admission) contract. Every field is optional
//! (absent is fine — a half-formed CLI invocation still reports) but NON-NULL when present
//! ([`RequiredPatchField`]: a present `null` is refused for every field). Named string
//! bounds are UTF-16 code units delegated to the sole primitive
//! [`within_utf16_max`](silverhand_backend_core::foundation::wire_text::within_utf16_max); `duration_ms`
//! is a nonnegative JS `number` (f64). The only effect is ONE
//! [`CliPreflightAuditSink::record`] call carrying a typed [`CliPreflightFailure`]; nothing is
//! persisted by this crate; the response is always `{ "ok": true }`.
//!
//! ## Records through a domain port, never the raw log
//!
//! The route depends on nothing but [`CliPreflightAuditSink`], injected as router state by the
//! composition root. It builds the domain fact from the self-pinned principal and the admitted
//! body and hands it to the port; where that fact becomes a durable, per-field-queryable log
//! line is the production sink's job (the composition owns the one
//! `CliPreflightFileSink`). This route therefore never touches `tracing`, JSON text, or a
//! file: it cannot flatten the fact into an unaddressable string, and the audit line's flat,
//! null-and-`[]`-preserving field shape is proven where it is produced — the sink's own crate,
//! and the production-composition test below that drives a request through the real sink.
//!
//! ## One governed deviation from the retired Hono route (ratified in #2250)
//!
//!  1. **Admission is strict, not lenient.** The Hono handler did
//!     `c.req.json().catch(() => ({}))`, so a missing body, malformed JSON, a missing
//!     `Content-Type`, or a non-object top-level shape all coerced to `{}` and answered
//!     `200 {ok:true}`. Under the frozen extractor those become the native `400`/`415`
//!     structural rejection and a non-object JSON shape (array/scalar) is the native `422`;
//!     a validation-rule failure is the minimal semantic `422 {"error":"invalid input"}`
//!     (the old Zod path returned `400` WITH an `issues` array — no endpoint publishes a
//!     validation-issue schema anymore). An empty OBJECT `{}` still answers `200`. The real
//!     CLI consumer always sends `POST` + `Content-Type: application/json` + a
//!     `JSON.stringify`'d object (`packages/cli/src/cli-trace.ts`), so it is unaffected.

use axum::{
    Json, Router,
    extract::{Extension, FromRequest, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::Deserialize;
use serde_json::json;
use silverhand_backend_core::{
    foundation::cli_preflight_audit::{
        CliPreflightAuditSink, CliPreflightFailure, NonNegativeFiniteF64,
    },
    foundation::wire_text,
    host_runtime::runtime_credential::RuntimePrincipal,
};

use crate::foundation::wire_admission::ValidatedJson;
use crate::foundation::wire_patch::RequiredPatchField;

/// The route path. The `{aid}` segment is what the self-pin layer binds; the handler takes
/// no path param and trusts `credential.agent()`.
pub const CLI_TRACE_PATH: &str = "/internal/agents/{aid}/cli-trace";

/// The unmapped-error body: a bare `text/plain` `500`, the same shape a thrown handler
/// yields — never a JSON envelope.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

/// Wide-string bound shared by `noun`, `verb`, `error_code`, `error_subkind`, and each
/// `flags_seen` element — the product's `z.string().max(64)` (UTF-16 code units).
const FIELD_MAX_UTF16: usize = 64;
/// The `error_message` bound — the product's `z.string().max(500)`.
const MESSAGE_MAX_UTF16: usize = 500;
/// The `flags_seen` array-length bound — the product's `z.array(...).max(64)`.
const FLAGS_MAX_LEN: usize = 64;

/// Mount the trace route over its one dependency: the [`CliPreflightAuditSink`] every
/// recorded failure flows through, injected as router state by the composition root. The
/// sink is a REQUIRED argument — there is no stateless form of this route, because a route
/// that could not record would silently swallow the audit fact. The self-pin credential
/// layer is composed in front by backend-app, exactly as for the profile read/write
/// siblings. Backend-app mounts this router behind that layer with the required
/// `CliPreflightFileSink` production backing.
pub fn cli_trace_router<S>(sink: S) -> Router
where
    S: CliPreflightAuditSink,
{
    Router::new()
        .route(CLI_TRACE_PATH, post(handle_cli_trace::<S>))
        .with_state(sink)
}

/// Every field is optional (`#[serde(default)]` → [`RequiredPatchField::Missing`]) but
/// non-null when present; unknown keys are ignored, and the [`ObjectOnly`](crate::foundation::wire_admission) wrapper inside
/// [`ValidatedJson`] rejects a non-object top-level shape before any field binds.
#[derive(Debug, Deserialize, Validate)]
struct CliTraceInput {
    #[serde(default)]
    #[garde(custom(required_within_utf16_max(FIELD_MAX_UTF16)))]
    noun: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(custom(required_within_utf16_max(FIELD_MAX_UTF16)))]
    verb: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(custom(required_flags_within_bounds(FIELD_MAX_UTF16, FLAGS_MAX_LEN)))]
    flags_seen: RequiredPatchField<Vec<String>>,
    #[serde(default)]
    #[garde(custom(required_within_utf16_max(FIELD_MAX_UTF16)))]
    error_code: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(custom(required_within_utf16_max(FIELD_MAX_UTF16)))]
    error_subkind: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(custom(required_within_utf16_max(MESSAGE_MAX_UTF16)))]
    error_message: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(custom(required_nonnegative))]
    duration_ms: RequiredPatchField<f64>,
}

async fn handle_cli_trace<S>(
    State(sink): State<S>,
    credential: Option<Extension<RuntimePrincipal>>,
    request: Request,
) -> Response
where
    S: CliPreflightAuditSink,
{
    // The credential (composition) verdict precedes body admission, exactly like the
    // profile-write sibling: a route reached without its self-pin layer is a wiring fault,
    // not a request the product can answer — and it must reach the sink zero times.
    let Some(Extension(principal)) = credential else {
        return composition_fault();
    };
    let input = match ValidatedJson::<CliTraceInput>::from_request(request, &()).await {
        Ok(ValidatedJson(input)) => input,
        Err(rejection) => return rejection.into_response(),
    };
    sink.record(build_failure(&principal, input));
    Json(json!({ "ok": true })).into_response()
}

/// Build the one [`CliPreflightFailure`] fact from the self-pinned principal and the admitted
/// body. `agent_id`/`conversation_id` come from the credential, never the wire; each absent
/// optional stays `None` (the sink projects it as explicit `null`) and an absent `flags_seen`
/// becomes the empty list (the sink projects `[]`, never omitted).
fn build_failure(principal: &RuntimePrincipal, input: CliTraceInput) -> CliPreflightFailure {
    CliPreflightFailure {
        agent_id: principal.agent().clone(),
        conversation_id: principal.source().cloned(),
        noun: present_string(input.noun),
        verb: present_string(input.verb),
        flags_seen: present_flags(input.flags_seen),
        error_code: present_string(input.error_code),
        error_subkind: present_string(input.error_subkind),
        error_message: present_string(input.error_message),
        duration_ms: present_duration(input.duration_ms),
    }
}

fn composition_fault() -> Response {
    tracing::error!("cli-trace route reached without its runtime credential");
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

// ---- presence → domain-field helpers (Missing carries the retired route's default) ----

/// A present string as `Some`, an absent field as `None` (the sink projects JSON `null`).
fn present_string(field: RequiredPatchField<String>) -> Option<String> {
    match field {
        RequiredPatchField::Value(value) => Some(value),
        RequiredPatchField::Missing => None,
    }
}

/// The present flags, or the empty list for an absent field (the sink projects JSON `[]`, the
/// retired route's `?? []` default — never omitted).
fn present_flags(field: RequiredPatchField<Vec<String>>) -> Vec<String> {
    match field {
        RequiredPatchField::Value(values) => values,
        RequiredPatchField::Missing => Vec::new(),
    }
}

/// A present `duration_ms` re-admitted into the domain's [`NonNegativeFiniteF64`] invariant,
/// an absent field as `None` (the sink projects JSON `null`). Admission's `required_nonnegative`
/// already refused a negative value, and JSON carries no NaN/Infinity token, so this
/// construction is always `Ok` for a value that reached here; `.ok()` keeps the projection
/// total rather than panicking if that ever ceased to hold.
fn present_duration(field: RequiredPatchField<f64>) -> Option<NonNegativeFiniteF64> {
    match field {
        RequiredPatchField::Value(value) => NonNegativeFiniteF64::new(value).ok(),
        RequiredPatchField::Missing => None,
    }
}

// ---- garde adapters — thin delegates to the sole value authority, no bound of their own --

/// Bound a present [`RequiredPatchField<String>`] to `max` UTF-16 code units, delegating the
/// count and comparison to [`within_utf16_max`](wire_text::within_utf16_max). `Missing`
/// carries no value and always passes.
fn required_within_utf16_max(
    max: usize,
) -> impl Fn(&RequiredPatchField<String>, &()) -> garde::Result {
    move |field, _| match field {
        RequiredPatchField::Value(value) if !wire_text::within_utf16_max(value, max) => {
            Err(garde::Error::new("string is too long"))
        }
        _ => Ok(()),
    }
}

/// Bound a present `flags_seen`: at most `len_max` elements, each at most `elem_max` UTF-16
/// code units (the product's `z.array(z.string().max(64)).max(64)`). Element length delegates
/// to the same sole primitive. `Missing` always passes.
fn required_flags_within_bounds(
    elem_max: usize,
    len_max: usize,
) -> impl Fn(&RequiredPatchField<Vec<String>>, &()) -> garde::Result {
    move |field, _| match field {
        RequiredPatchField::Value(values) => {
            if values.len() > len_max {
                return Err(garde::Error::new("too many flags"));
            }
            if values
                .iter()
                .any(|value| !wire_text::within_utf16_max(value, elem_max))
            {
                return Err(garde::Error::new("flag is too long"));
            }
            Ok(())
        }
        RequiredPatchField::Missing => Ok(()),
    }
}

/// Reject a present negative `duration_ms` — the product's `z.number().nonnegative()` on a
/// JS `number`. JSON carries no NaN/Infinity token (serde_json refuses both at decode), so a
/// present value is always a finite `f64` and a plain `< 0.0` is the whole contract.
/// `Missing` always passes.
fn required_nonnegative(field: &RequiredPatchField<f64>, _: &()) -> garde::Result {
    match field {
        RequiredPatchField::Value(value) if *value < 0.0 => {
            Err(garde::Error::new("duration must be nonnegative"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request as HttpRequest, StatusCode, header::CONTENT_TYPE};
    use serde_json::Value;
    use silverhand_backend_core::{
        foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
        host_runtime::runtime_credential::{
            RuntimeCustody, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
        },
    };
    use silverhand_backend_filesystem::foundation::audit_sink::{AppTag, AuditClock, CliAuditFile};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    // ---- a recording sink double: it captures the exact typed facts the route records, so
    //      a test pins both WHAT was recorded and HOW MANY TIMES (exactly-one / zero-reach). --

    #[derive(Clone, Default)]
    struct RecordingSink(Arc<Mutex<Vec<CliPreflightFailure>>>);

    impl RecordingSink {
        fn recorded(&self) -> Vec<CliPreflightFailure> {
            self.0.lock().unwrap().clone()
        }
    }

    impl CliPreflightAuditSink for RecordingSink {
        fn record(&self, failure: CliPreflightFailure) {
            self.0.lock().unwrap().push(failure);
        }
    }

    // ---- a pinned clock for the production-sink composition test ----

    #[derive(Clone)]
    struct FixedClock(i64);

    impl AuditClock for FixedClock {
        fn now_unix_millis(&self) -> i64 {
            self.0
        }
    }

    // ---- credential builders ----

    fn custody() -> RuntimeCustody {
        RuntimeCustody::new(
            RuntimeGeneration::new(1).unwrap(),
            DaemonInstanceId::parse("daemon-1").unwrap(),
        )
    }

    /// A conversation-slot principal — carries a source conversation.
    fn conversation_principal(agent: &str, source: &str) -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::ConversationSlot {
                agent: AgentId::parse(agent).unwrap(),
                source: ConversationId::parse(source).unwrap(),
            },
            custody(),
        )
    }

    /// An agent-slot principal — no source conversation (records `conversation_id: None`).
    fn agent_principal(agent: &str) -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::AgentSlot {
                agent: AgentId::parse(agent).unwrap(),
            },
            custody(),
        )
    }

    // ---- request + run builders ----

    fn trace_request(principal: RuntimePrincipal, body: &'static str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("POST")
            .uri("/internal/agents/a1/cli-trace")
            .header(CONTENT_TYPE, "application/json")
            .extension(principal)
            .body(Body::from(body))
            .unwrap()
    }

    /// Drive a request through the route wired over `sink`, returning the wire outcome.
    async fn run_with<S>(sink: S, request: HttpRequest<Body>) -> (StatusCode, Value)
    where
        S: CliPreflightAuditSink,
    {
        let response = cli_trace_router(sink).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    // ================= success + the exact recorded fact =================

    #[tokio::test]
    async fn a_full_report_answers_ok_and_records_exactly_one_fact_with_every_field() {
        let sink = RecordingSink::default();
        let (status, body) = run_with(
            sink.clone(),
            trace_request(
                conversation_principal("a1", "conv-9"),
                r#"{"noun":"message","verb":"send","flags_seen":["--target","--json"],
                    "error_code":"validation_error","error_subkind":"unknown_flag",
                    "error_message":"unknown flag --nope","duration_ms":12.5}"#,
            ),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "ok": true }));

        let recorded = sink.recorded();
        assert_eq!(recorded.len(), 1, "exactly one preflight failure recorded");
        assert_eq!(
            recorded[0],
            CliPreflightFailure {
                agent_id: AgentId::parse("a1").unwrap(),
                conversation_id: Some(ConversationId::parse("conv-9").unwrap()),
                noun: Some("message".to_owned()),
                verb: Some("send".to_owned()),
                flags_seen: vec!["--target".to_owned(), "--json".to_owned()],
                error_code: Some("validation_error".to_owned()),
                error_subkind: Some("unknown_flag".to_owned()),
                error_message: Some("unknown flag --nope".to_owned()),
                duration_ms: Some(NonNegativeFiniteF64::new(12.5).unwrap()),
            }
        );
    }

    #[tokio::test]
    async fn an_empty_object_answers_ok_and_records_every_default() {
        // The retired route's `catch(()=>({}))` leniency is gone, but an empty OBJECT is a
        // valid all-absent report: 200, and every optional carries its default (None / []).
        let sink = RecordingSink::default();
        let (status, body) =
            run_with(sink.clone(), trace_request(agent_principal("a1"), r#"{}"#)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "ok": true }));

        let recorded = sink.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0],
            CliPreflightFailure {
                agent_id: AgentId::parse("a1").unwrap(),
                // AgentSlot carries no source conversation → None (sink projects null).
                conversation_id: None,
                noun: None,
                verb: None,
                flags_seen: Vec::new(),
                error_code: None,
                error_subkind: None,
                error_message: None,
                duration_ms: None,
            }
        );
    }

    // ================= admission falsifiers (all 422, zero sink reach) =================

    /// The sharp array bypass: `["message"]` is a single-element array whose element is a
    /// valid value for the first field. `ObjectOnly` rejects it structurally before any bind,
    /// so no fact is built and the sink is reached zero times.
    #[tokio::test]
    async fn a_value_shaped_array_body_is_422_with_zero_reach() {
        let sink = RecordingSink::default();
        let (status, _body) = run_with(
            sink.clone(),
            trace_request(agent_principal("a1"), r#"["message"]"#),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            sink.recorded().is_empty(),
            "a rejected body builds no fact and never reaches the sink"
        );
    }

    #[tokio::test]
    async fn a_present_null_field_is_rejected_with_zero_reach() {
        // Every field is optional but NON-null when present: `noun: null` is refused.
        let sink = RecordingSink::default();
        let (status, _body) = run_with(
            sink.clone(),
            trace_request(agent_principal("a1"), r#"{"noun":null}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(sink.recorded().is_empty());
    }

    #[tokio::test]
    async fn an_over_bound_astral_plane_string_is_rejected() {
        // 33 astral-plane scalars = 66 UTF-16 code units > the 64 bound on `noun`. Pins the
        // UTF-16 (not byte, not scalar) accounting delegated to the shared primitive.
        let body: &'static str =
            r#"{"noun":"😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀"}"#;
        let (status, _body) = run_with(
            RecordingSink::default(),
            trace_request(agent_principal("a1"), body),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_boundary_length_string_is_accepted() {
        // Exactly 64 UTF-16 units (32 astral-plane scalars) is within bound → 200.
        let body: &'static str =
            r#"{"noun":"😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀"}"#;
        let (status, response) = run_with(
            RecordingSink::default(),
            trace_request(agent_principal("a1"), body),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response, json!({ "ok": true }));
    }

    #[tokio::test]
    async fn a_negative_duration_is_rejected() {
        let (status, _body) = run_with(
            RecordingSink::default(),
            trace_request(agent_principal("a1"), r#"{"duration_ms":-1}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_zero_duration_is_accepted() {
        // `.nonnegative()` admits zero.
        let (status, _body) = run_with(
            RecordingSink::default(),
            trace_request(agent_principal("a1"), r#"{"duration_ms":0}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn too_many_flags_are_rejected() {
        // 65 elements > the 64 array bound.
        let flags = (0..65).map(|_| "\"x\"").collect::<Vec<_>>().join(",");
        let body = format!(r#"{{"flags_seen":[{flags}]}}"#);
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/internal/agents/a1/cli-trace")
            .header(CONTENT_TYPE, "application/json")
            .extension(agent_principal("a1"))
            .body(Body::from(body))
            .unwrap();
        let response = cli_trace_router(RecordingSink::default())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_wrong_typed_field_keeps_the_native_reject() {
        // `duration_ms` as a string is a native decode fault (422), never the handler.
        let (status, _body) = run_with(
            RecordingSink::default(),
            trace_request(agent_principal("a1"), r#"{"duration_ms":"fast"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_missing_content_type_keeps_the_native_415() {
        // The tightened admission: no lenient coercion — a missing Content-Type is the
        // native structural rejection, not a `200`.
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/internal/agents/a1/cli-trace")
            .extension(agent_principal("a1"))
            .body(Body::from(r#"{"noun":"send"}"#))
            .unwrap();
        let response = cli_trace_router(RecordingSink::default())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    // ================= self-pin wiring (zero sink reach on refusal/fault) =================

    #[tokio::test]
    async fn an_absent_credential_is_a_safe_500_and_records_nothing() {
        let sink = RecordingSink::default();
        // No credential extension: the self-pin layer was not composed in front.
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/internal/agents/a1/cli-trace")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"noun":"send"}"#))
            .unwrap();
        let response = cli_trace_router(sink.clone())
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            sink.recorded().is_empty(),
            "a composition fault records no preflight fact"
        );
    }

    /// Supplies a refusal to test the middleware-to-handler boundary, not the
    /// production verifier's own subject-binding policy.
    #[derive(Clone)]
    struct PinnedAuthority {
        enrolled: AgentId,
    }
    impl crate::host_runtime::runtime_credential::AuthenticateRuntimeCredential for PinnedAuthority {
        async fn authenticate_runtime(
            &self,
            _bearer: crate::host_runtime::runtime_credential::RuntimeBearer,
            route_agent: AgentId,
        ) -> Result<
            Option<RuntimePrincipal>,
            silverhand_backend_core::foundation::persistence::PersistenceFailure,
        > {
            if route_agent == self.enrolled {
                Ok(Some(agent_principal(self.enrolled.as_str())))
            } else {
                Ok(None)
            }
        }
    }

    #[tokio::test]
    async fn an_authority_refusal_is_401_before_the_handler_with_zero_reach() {
        use axum::http::header::AUTHORIZATION;

        let sink = RecordingSink::default();
        let authority = PinnedAuthority {
            enrolled: AgentId::parse("a1").unwrap(),
        };
        let router = crate::host_runtime::runtime_credential::require_runtime_credential(
            cli_trace_router(sink.clone()),
            authority,
        );
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/internal/agents/someone-else/cli-trace")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer test-secret")
            .body(Body::from(r#"{"noun":"send"}"#))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            sink.recorded().is_empty(),
            "a non-self caller never reaches the handler or the sink"
        );
    }

    // ============ production composition: the route + the REAL file sink ============

    /// The route wired over the production [`CliPreflightFileSink`] writes ONE audit line
    /// whose business fields are each an independently-addressable top-level key with the
    /// live casing — `category`/`error_subkind` addressable, `flags_seen` a JSON array,
    /// absent optionals explicit `null`. This asserts addressability DIRECTLY on the real
    /// on-disk line (the line IS the object), never by parsing a string field's inner JSON.
    /// It proves the handler builds the fact correctly AND the composition projects it
    /// queryably — the end-to-end contract a mounted route must not regress.
    #[tokio::test]
    async fn the_route_over_the_production_sink_writes_one_addressable_line() {
        // 2023-11-14T22:13:20.123Z in milliseconds — a pinned instant for a stable filename.
        const ANCHOR_MS: i64 = 1_700_000_123_123;

        let dir = tempfile::tempdir().unwrap();
        let owner = CliAuditFile::open(
            dir.path().to_path_buf(),
            AppTag::Server,
            FixedClock(ANCHOR_MS),
        )
        .await
        .unwrap();
        let sink = owner.preflight_sink();

        let (status, body) = run_with(
            sink.clone(),
            trace_request(
                conversation_principal("a1", "conv-9"),
                r#"{"noun":"message","flags_seen":["--target"],
                    "error_subkind":"unknown_flag","duration_ms":12.5}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "ok": true }));

        // Drain + close so the accepted line is durably on disk before we read it.
        owner.close().await;

        // The one file for this boot; read its single line and address the flat top-level keys.
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let path = entries.next_entry().await.unwrap().unwrap().path();
        assert!(entries.next_entry().await.unwrap().is_none());
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "one request records exactly one line");
        let object: Value = serde_json::from_str(lines[0]).unwrap();

        assert_eq!(object["category"], "cli.preflight_failure");
        assert_eq!(object["agentId"], "a1");
        assert_eq!(object["conversationId"], "conv-9");
        assert_eq!(object["noun"], "message");
        assert_eq!(object.get("verb"), Some(&Value::Null));
        assert_eq!(
            object["flags_seen"],
            json!(["--target"]),
            "flags_seen is a JSON array, not a string"
        );
        assert_eq!(
            object["error_subkind"], "unknown_flag",
            "the daily-report group-by key stays addressable"
        );
        assert_eq!(object.get("error_code"), Some(&Value::Null));
        assert_eq!(object["duration_ms"], json!(12.5));
        assert_eq!(object["status"], "failure");
    }
}
