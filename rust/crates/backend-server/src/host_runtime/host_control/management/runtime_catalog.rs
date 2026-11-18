//! `GET /api/hosts/{id}/runtime-catalog`.

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    foundation::identity::{HostId, UserId},
    host_runtime::read::{ComputerReadModel, find_owned_computer},
    identity_access::agents::runtime::RuntimeKind,
};

use crate::{
    application_orchestration::daemon_delivery::OwningHostDataPlane,
    foundation::error,
    host_runtime::host_control::runtime_catalog::{
        self, DetectedRuntime, RuntimeCatalogOutcome, RuntimeCatalogRequest,
    },
};

use super::internal_error;

pub const HOST_RUNTIME_CATALOG_PATH: &str = "/api/hosts/{id}/runtime-catalog";

#[derive(Clone)]
struct CatalogState<R> {
    computers: R,
    data_plane: OwningHostDataPlane,
}

pub fn router<R>(computers: R, data_plane: OwningHostDataPlane) -> Router
where
    R: ComputerReadModel,
{
    Router::new()
        .route(HOST_RUNTIME_CATALOG_PATH, get(catalog::<R>))
        .with_state(CatalogState {
            computers,
            data_plane,
        })
}

async fn catalog<R>(
    Path(raw_host): Path<String>,
    Query(query): Query<ForceQuery>,
    principal: Option<Extension<UserId>>,
    State(state): State<CatalogState<R>>,
) -> Response
where
    R: ComputerReadModel,
{
    let Some(Extension(requester)) = principal else {
        tracing::error!("host-runtime-catalog route reached without its human principal fact");
        return internal_error();
    };
    let Ok(host) = HostId::parse(raw_host) else {
        return not_found();
    };
    let record = match state.computers.read_computer(host.clone()).await {
        Ok(record) => record,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "host-runtime-catalog read failed"
            );
            return internal_error();
        }
    };
    let Some(owned) = find_owned_computer(&requester, record) else {
        return not_found();
    };
    if owned.record().revoked_at.is_some() {
        return coded("host has been revoked", "host_revoked", 409);
    }
    if owned.record().finalized_at.is_none() {
        return coded("host is not active", "host_not_active", 409);
    }
    let models_for = match query.models_for.as_deref() {
        None => None,
        Some(raw) => match RuntimeKind::parse(raw) {
            Some(runtime) => Some(runtime),
            None => return coded("unknown runtime", "validation_error", 400),
        },
    };
    let custody = match state.data_plane.resolve(&host) {
        Ok(Some(custody)) => custody,
        Ok(None) => return coded("computer is offline", "computer_offline", 503),
        Err(cause) => {
            tracing::error!(
                error = crate::foundation::safe_log_field::safe_display(&(cause)),
                "owning-host data plane resolve failed"
            );
            return coded("daemon request failed", "daemon_internal_error", 502);
        }
    };
    let coordinate = custody.clone();
    match runtime_catalog::probe(
        &state.data_plane,
        RuntimeCatalogRequest::new(custody, truthy(query.force.as_deref()))
            .with_models_for(models_for),
    )
    .await
    {
        RuntimeCatalogOutcome::Detected(catalog) => {
            let Ok(view) = catalog.rows_for(&coordinate) else {
                return internal_error();
            };
            Json(CatalogResponse {
                runtimes: view.runtimes.iter().map(RuntimeDto::from).collect(),
                detected_at: view.detected_at,
                model_discovery: view.model_discovery.cloned(),
            })
            .into_response()
        }
        RuntimeCatalogOutcome::AttachPending => {
            coded("computer is offline", "computer_offline", 503)
        }
        RuntimeCatalogOutcome::Timeout => coded("daemon request timed out", "daemon_timeout", 504),
        RuntimeCatalogOutcome::DaemonInternalError => {
            coded("daemon request failed", "daemon_internal_error", 502)
        }
        RuntimeCatalogOutcome::TransportContractViolation => internal_error(),
    }
}

#[derive(Deserialize)]
struct ForceQuery {
    force: Option<String>,
    models_for: Option<String>,
}

fn truthy(value: Option<&str>) -> bool {
    matches!(value, Some("1" | "true"))
}

#[derive(Serialize)]
struct CatalogResponse {
    runtimes: Vec<RuntimeDto>,
    detected_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_discovery: Option<runtime_catalog::ModelDiscovery>,
}

#[derive(Serialize)]
struct RuntimeDto {
    kind: &'static str,
    state: &'static str,
    reason: &'static str,
    installed: bool,
    binary: Option<String>,
    version: Option<String>,
}

impl From<&DetectedRuntime> for RuntimeDto {
    fn from(runtime: &DetectedRuntime) -> Self {
        Self {
            kind: runtime.kind.as_str(),
            state: runtime.state.as_str(),
            reason: runtime.reason.as_str(),
            installed: runtime.installed,
            binary: runtime.binary.clone(),
            version: runtime.version.clone(),
        }
    }
}

fn not_found() -> Response {
    coded("host not found", "host_not_found", 404)
}

fn coded(message: &'static str, code: &'static str, status: u16) -> Response {
    (
        axum::http::StatusCode::from_u16(status).unwrap(),
        Json(serde_json::json!({ "error": message, "error_code": code })),
    )
        .into_response()
}
