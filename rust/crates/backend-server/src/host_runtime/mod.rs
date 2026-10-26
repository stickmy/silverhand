//! Host lifecycle routes and adapters.
//!
//! All eight production-settled targets now live only under this namespace; the temporary
//! crate-root migration facades have been removed.
//! The resolved `local_runtime_catalog` HTTP leaf reports this process's coordinate-free machine
//! detection and remains distinct from the remote, daemon-coordinate-bound host-control catalog.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    host_runtime::{
        environment::{Clock, EntropySource},
        invite::{CreateHostInvite, HostInviteError, HostInviteUnitOfWork},
    },
};

use crate::foundation::error;
use crate::foundation::wire_admission::ValidatedJson;

pub mod agent_clear_session;
pub mod agent_materialize;
pub mod agent_presence;
pub mod agent_runtime_state;
pub mod agent_runtime_switch;
pub mod agent_stop;
pub mod availability;
pub mod cascade_effects;
pub mod claim;
pub(crate) mod daemon_runtime_codecs;
pub(crate) mod daemon_runtime_owner;
pub(crate) mod daemon_runtime_transport;
pub mod delete;
pub mod detail;
pub mod host_agent_background;
pub mod host_attachment;
pub mod host_control;
pub mod host_resources;
pub(crate) mod host_wire;
pub mod list;
pub mod local_runtime_catalog;
pub mod resolve;
pub mod revoke;
pub mod rotate_connect_token;
pub mod rotate_token;
pub mod runtime_control;
pub mod runtime_credential;
pub(crate) mod runtime_event_settlement;
pub use runtime_event_settlement::{ConversationRunTerminalizeError, ConversationRunTerminalizer};
pub mod runtime_status_publisher;
pub mod scope_event_publishers;
pub mod scope_runtime_stop;

use host_wire::HostDto;

pub const HOSTS_PATH: &str = "/api/hosts";
pub const DEFAULT_DAEMON_NPM_SPEC: &str = "@silverhand-ai/daemon@latest";

struct StateInner<U, R, C> {
    create_host_invite: CreateHostInvite<U, R, C>,
    daemon_npm_spec: String,
}

struct HostRuntimeState<U, R, C>(Arc<StateInner<U, R, C>>);

impl<U, R, C> Clone for HostRuntimeState<U, R, C> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<U, R, C>(
    create_host_invite: CreateHostInvite<U, R, C>,
    daemon_npm_spec: Option<String>,
) -> Router
where
    U: HostInviteUnitOfWork,
    R: EntropySource + 'static,
    C: Clock + 'static,
{
    let daemon_npm_spec = resolve_daemon_npm_spec(daemon_npm_spec);
    let state = HostRuntimeState(Arc::new(StateInner {
        create_host_invite,
        daemon_npm_spec,
    }));
    Router::new()
        .route(HOSTS_PATH, post(create_host::<U, R, C>))
        .with_state(state)
}

/// Resolve the daemon package spec the connect-command card shows: a configured value,
/// trimmed, or the default when it is absent or blank. Shared by the create-invite response and
/// the connect-token rotation response so the two can never show a different spec.
pub(super) fn resolve_daemon_npm_spec(configured: Option<String>) -> String {
    configured
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_DAEMON_NPM_SPEC.to_owned())
}

async fn create_host<U, R, C>(
    State(state): State<HostRuntimeState<U, R, C>>,
    principal: Option<Extension<UserId>>,
    ValidatedJson(_body): ValidatedJson<CreateHostBody>,
) -> Response
where
    U: HostInviteUnitOfWork,
    R: EntropySource + 'static,
    C: Clock + 'static,
{
    let Some(Extension(user_id)) = principal else {
        tracing::error!("host invite route reached without its human principal fact");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
    };

    match state.0.create_host_invite.execute(user_id).await {
        Ok(fact) => {
            let response = HostCreateResponse {
                host_id: fact.host.id().to_owned(),
                host: HostDto::from(fact.host),
                connect_token: fact.connect_token.reveal_once(),
                daemon_npm_spec: state.0.daemon_npm_spec.clone(),
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(error) => BoundaryError::from(error).into_response(),
    }
}

/// The create-host body carries no fields: the invite is minted entirely from the
/// authenticated principal, so the sole valid body is the empty object `{}`. Decoding
/// runs through the frozen [`ValidatedJson`] extractor, whose shared object-only floor
/// commits the decode to a JSON object BEFORE the empty struct's positional seq decode —
/// so a bare `[]` can never decode to this all-default struct and mint an invite. A
/// malformed body, a wrong top-level shape, and any stray key keep axum's native `Json`
/// classification (`400`/`422`/`415`) — this route publishes no validation-issue schema of
/// its own. `deny_unknown_fields` turns a stray key into a native data fault (`422`).
#[derive(Debug, Deserialize, Validate)]
#[serde(deny_unknown_fields)]
struct CreateHostBody {}

/// The anti-enumeration refusal shared by every host route that names a computer: an
/// absent computer and one the requester does not own collapse to the same `404`, so
/// no surface becomes an existence oracle for computer ids. Pinned to the TypeScript
/// handlers (`{ error: 'host not found', error_code: 'host_not_found' }`), so a drift
/// shows here rather than route by route.
pub(super) const HOST_NOT_FOUND: &str = "host not found";
pub(super) const HOST_NOT_FOUND_CODE: &str = "host_not_found";

/// The unmapped-failure body: these routes register no error handler, so an
/// unexpected failure falls to the framework default — a plain-text `500`, never a
/// JSON envelope.
pub(super) const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

/// A refusal that carries a machine-readable `error_code` beside its message. Shared
/// by the host routes whose refusals the renderer branches on.
#[derive(Serialize)]
pub(super) struct ErrorBody {
    pub error: &'static str,
    pub error_code: &'static str,
}

pub(super) fn host_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: HOST_NOT_FOUND,
            error_code: HOST_NOT_FOUND_CODE,
        }),
    )
        .into_response()
}

pub(super) fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

#[derive(Serialize)]
struct HostCreateResponse {
    host_id: String,
    host: HostDto,
    connect_token: String,
    daemon_npm_spec: String,
}

struct BoundaryError(Box<dyn std::error::Error + Send + Sync>);

impl From<PersistenceFailure> for BoundaryError {
    fn from(error: PersistenceFailure) -> Self {
        Self(Box::new(error))
    }
}

impl From<HostInviteError> for BoundaryError {
    fn from(error: HostInviteError) -> Self {
        Self(Box::new(error))
    }
}

impl IntoResponse for BoundaryError {
    fn into_response(self) -> Response {
        tracing::error!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &(error::describe(self.0.as_ref()))
            ),
            "create host invite failed"
        );
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        convert::Infallible,
        io::{self, Write},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use silverhand_backend_core::host_runtime::{
        custody::NewLongLivedCredential,
        invite::{HostInviteWriteError, NewUserInviteHost},
    };
    use tower::ServiceExt;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[derive(Clone, Copy, Default)]
    enum StoreBehavior {
        #[default]
        Succeed,
        AlwaysDuplicate,
    }

    #[derive(Clone, Default)]
    struct CountingStore {
        behavior: StoreBehavior,
        calls: Arc<AtomicUsize>,
    }

    impl CountingStore {
        fn with_behavior(behavior: StoreBehavior) -> Self {
            Self {
                behavior,
                ..Self::default()
            }
        }
    }

    impl HostInviteUnitOfWork for CountingStore {
        async fn create_host_and_credential(
            &self,
            _host: NewUserInviteHost,
            _credential: NewLongLivedCredential,
        ) -> Result<(), HostInviteWriteError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.behavior {
                StoreBehavior::AlwaysDuplicate => {
                    return Err(HostInviteWriteError::DuplicateHostName);
                }
                StoreBehavior::Succeed => {}
            }
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FixedEntropy(Arc<Mutex<VecDeque<u8>>>);

    impl FixedEntropy {
        fn sequence() -> Self {
            Self(Arc::new(Mutex::new(
                (0..1024).map(|index| index as u8).collect(),
            )))
        }
    }

    impl EntropySource for FixedEntropy {
        type Error = Infallible;

        fn fill(&self, destination: &mut [u8]) -> Result<(), Self::Error> {
            let mut source = self.0.lock().unwrap();
            for byte in destination {
                *byte = source.pop_front().unwrap();
            }
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    struct FixedClock;

    impl Clock for FixedClock {
        type Error = Infallible;

        fn unix_time_ms(&self) -> Result<i64, Self::Error> {
            Ok(1_700_000_000_123)
        }
    }

    #[derive(Clone)]
    struct FailingEntropy;

    #[derive(Debug, thiserror::Error)]
    #[error("entropy unavailable")]
    struct EntropyUnavailable;

    impl EntropySource for FailingEntropy {
        type Error = EntropyUnavailable;

        fn fill(&self, _destination: &mut [u8]) -> Result<(), Self::Error> {
            Err(EntropyUnavailable)
        }
    }

    #[derive(Clone, Copy)]
    struct FailingClock;

    #[derive(Debug, thiserror::Error)]
    #[error("clock unavailable")]
    struct ClockUnavailable;

    impl Clock for FailingClock {
        type Error = ClockUnavailable;

        fn unix_time_ms(&self) -> Result<i64, Self::Error> {
            Err(ClockUnavailable)
        }
    }

    fn test_router(store: CountingStore) -> Router {
        test_router_with_spec(store, None)
    }

    fn test_router_with_spec(store: CountingStore, daemon_npm_spec: Option<String>) -> Router {
        router(
            CreateHostInvite::new(store, FixedEntropy::sequence(), FixedClock),
            daemon_npm_spec,
        )
    }

    fn user() -> UserId {
        UserId::parse("user-1").unwrap()
    }

    async fn response(app: Router, principal: Option<UserId>, body: &str) -> (StatusCode, String) {
        let mut request = Request::post(HOSTS_PATH)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap();
        if let Some(principal) = principal {
            request.extensions_mut().insert(principal);
        }
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_missing_principal_fails_closed_before_the_uow() {
        let store = CountingStore::default();
        let (status, body) = response(test_router(store.clone()), None, "{}").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
        assert_eq!(store.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unknown_key_is_a_native_data_fault_and_never_reaches_the_uow() {
        // `deny_unknown_fields` turns any stray key into a native serde DATA fault: the
        // frozen extractor forwards axum's classification (422 today) verbatim — no issue
        // array, no semantic envelope — and the UoW is never entered.
        let store = CountingStore::default();
        let (status, body) = response(
            test_router(store.clone()),
            Some(user()),
            "{\"zeta\":true,\"alpha\":true}",
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_ne!(body, "{\"error\":\"invalid input\"}");
        assert_eq!(store.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_non_object_shape_is_a_native_422_and_never_reaches_the_uow() {
        // The shared object-only floor commits the decode to a JSON object, so EVERY
        // top-level non-object — the empty array `[]` (which a bare empty struct would have
        // decoded as `Ok` and MINTED a host invite), a populated array, or any scalar — is a
        // uniform native data fault (`422`) refused before the empty struct's positional seq
        // decode. Never our semantic envelope, and the UoW is never entered.
        for raw in ["[]", "[1,2]", "\"text\"", "42", "true", "null"] {
            let store = CountingStore::default();
            let (status, body) = response(test_router(store.clone()), Some(user()), raw).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "shape {raw:?} status {status}"
            );
            assert_ne!(body, "{\"error\":\"invalid input\"}", "shape {raw:?}");
            assert_eq!(store.calls.load(Ordering::SeqCst), 0, "shape {raw:?}");
        }
    }

    #[tokio::test]
    async fn malformed_or_empty_json_is_a_native_400_and_never_reaches_the_uow() {
        // The old TS empty-object fallback is gone: a malformed body is a native syntax
        // fault (400), not a silent coercion to `{}`.
        for raw in ["not-json", ""] {
            let store = CountingStore::default();
            let (status, _) = response(test_router(store.clone()), Some(user()), raw).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(store.calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn a_missing_content_type_is_a_native_415_and_never_reaches_the_uow() {
        // Native `Json` requires the `application/json` content-type; its absence is a
        // native `415` before the UoW (the old fallback accepted any content-type).
        let store = CountingStore::default();
        let mut request = Request::post(HOSTS_PATH).body(Body::from("{}")).unwrap();
        request.extensions_mut().insert(user());
        let response = test_router(store.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(store.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn trims_a_configured_daemon_package_spec() {
        let store = CountingStore::default();
        let (status, body) = response(
            test_router_with_spec(
                store,
                Some("  file:/repo/packages/silverhand-daemon  ".into()),
            ),
            Some(user()),
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["daemon_npm_spec"],
            "file:/repo/packages/silverhand-daemon"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn entropy_failure_is_one_safe_boundary_refusal_before_the_uow() {
        let store = CountingStore::default();
        let app = router(
            CreateHostInvite::new(store.clone(), FailingEntropy, FixedClock),
            None,
        );
        assert_safe_failure(app, &store, 0, "entropy unavailable").await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clock_failure_is_one_safe_boundary_refusal_before_the_uow() {
        let store = CountingStore::default();
        let app = router(
            CreateHostInvite::new(store.clone(), FixedEntropy::sequence(), FailingClock),
            None,
        );
        assert_safe_failure(app, &store, 0, "clock unavailable").await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn name_exhaustion_is_one_safe_boundary_refusal_after_eight_attempts() {
        let store = CountingStore::with_behavior(StoreBehavior::AlwaysDuplicate);
        let app = test_router(store.clone());
        assert_safe_failure(app, &store, 8, "unable to allocate a unique host name").await;
    }

    async fn assert_safe_failure(
        app: Router,
        store: &CountingStore,
        expected_calls: usize,
        expected_chain: &str,
    ) {
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_target(false)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let (status, body) = response(app, Some(user()), "{}").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
        assert_eq!(store.calls.load(Ordering::SeqCst), expected_calls);

        let errors = logs
            .events()
            .into_iter()
            .filter(|event| event["level"] == "ERROR")
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1);
        let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
        assert!(chain.contains(expected_chain));
        assert!(!chain.contains("HB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs"));
    }

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl LogBuffer {
        fn events(&self) -> Vec<serde_json::Value> {
            let bytes = self.0.lock().unwrap().clone();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    struct LogWriter(LogBuffer);

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for LogBuffer {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            LogWriter(self.clone())
        }
    }
}
