//! `GET /api/hosts/{id}/status` — persisted registration plus memory-only host facts.
//! `POST /api/hosts/{id}/status/refresh` — same answer after re-probing the attached
//! daemon; the GET path never round-trips to a daemon.
//!
//! The owner reads `view: "privileged"`. Any other signed-in human may GET
//! `view: "public"`: OS only from the snapshot, no unreachable reason, no admission
//! failure. Refresh probes a daemon, so it stays owner-only.

use std::collections::HashSet;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::{AgentId, HostId, UserId},
    host_runtime::status::{HostStatusAgentFact, HostStatusStoredReadModel, readable_host_status},
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::capability::read_tier::ReadTier,
};

use crate::{
    foundation::error,
    host_runtime::host_control::{
        admission::HostAdmission,
        status_seed::{HostStatusSeeds, RefreshOutcome, SeedRead, SeededStatus},
        status_snapshot::Usage,
        wire_number::js_safe_integer,
    },
};

use super::internal_error;

pub const HOST_STATUS_PATH: &str = "/api/hosts/{id}/status";
pub const HOST_STATUS_REFRESH_PATH: &str = "/api/hosts/{id}/status/refresh";

#[derive(Clone)]
struct StatusState<R> {
    stored: R,
    seeds: HostStatusSeeds,
    admission: HostAdmission,
}

pub fn router<R>(stored: R, seeds: HostStatusSeeds, admission: HostAdmission) -> Router
where
    R: HostStatusStoredReadModel,
{
    Router::new()
        .route(HOST_STATUS_PATH, get(status::<R>))
        .route(HOST_STATUS_REFRESH_PATH, post(refresh::<R>))
        .with_state(StatusState {
            stored,
            seeds,
            admission,
        })
}

async fn status<R>(
    Path(raw_host): Path<String>,
    principal: Option<Extension<UserId>>,
    State(state): State<StatusState<R>>,
) -> Response
where
    R: HostStatusStoredReadModel,
{
    respond(raw_host, principal, state, false).await
}

async fn refresh<R>(
    Path(raw_host): Path<String>,
    principal: Option<Extension<UserId>>,
    State(state): State<StatusState<R>>,
) -> Response
where
    R: HostStatusStoredReadModel,
{
    respond(raw_host, principal, state, true).await
}

/// The tier, revocation and admission are decided before any probe, so a refresh
/// never reaches a daemon the viewer does not own.
async fn respond<R>(
    raw_host: String,
    principal: Option<Extension<UserId>>,
    state: StatusState<R>,
    probe: bool,
) -> Response
where
    R: HostStatusStoredReadModel,
{
    let Some(Extension(viewer)) = principal else {
        tracing::error!("host-status route reached without its human principal fact");
        return internal_error();
    };
    let Ok(host) = HostId::parse(raw_host) else {
        return not_found();
    };
    let facts = match state.stored.read_host_status(&host).await {
        Ok(facts) => facts,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "host-status stored read failed"
            );
            return internal_error();
        }
    };
    let visible = match readable_host_status(&viewer, &host, facts.as_ref()) {
        Ok(Some(visible)) if visible.tier == ReadTier::Privileged || !probe => visible,
        Ok(_) => return not_found(),
        Err(cause) => {
            tracing::error!(
                error = crate::foundation::safe_log_field::safe_display(&(cause)),
                "host-status facts were cross-wired"
            );
            return internal_error();
        }
    };
    let tier = visible.tier;
    let computer = visible.computer;
    if computer
        .last_seen_at
        .is_some_and(|value| !js_safe_integer(value))
    {
        tracing::error!(
            host_id = crate::foundation::safe_log_field::safe_display(&(host.as_str())),
            "host last-seen timestamp is not JS-safe"
        );
        return internal_error();
    }
    if computer.revoked_at.is_some() {
        return render(
            tier,
            StatusResponse::Revoked {
                host_id: host.as_str().to_owned(),
                agents: agent_rows(&visible.agents, None),
                last_seen_at: computer.last_seen_at,
            },
        );
    }

    let refusal = match state.admission.refusal(&host) {
        Ok(refusal) => refusal,
        Err(cause) => {
            tracing::error!(
                error = crate::foundation::safe_log_field::safe_display(&(cause)),
                "host admission refusal read failed"
            );
            return internal_error();
        }
    };
    if let Some(refusal) = refusal {
        if !js_safe_integer(refusal.observed_at) {
            tracing::error!(
                host_id = crate::foundation::safe_log_field::safe_display(&(host.as_str())),
                "host admission timestamp is not JS-safe"
            );
            return internal_error();
        }
        return render(
            tier,
            StatusResponse::Disconnected {
                host_id: host.as_str().to_owned(),
                agents: agent_rows(&visible.agents, None),
                last_seen_at: computer.last_seen_at,
                admission_failure: Some(AdmissionFailure {
                    kind: "protocol_mismatch",
                    offered_protocol_version: refusal.offered_protocol_version,
                    expected_protocol_version: refusal.expected_protocol_version,
                    observed_at: refusal.observed_at,
                }),
            },
        );
    }

    if probe {
        match state.seeds.refresh(&host).await {
            Ok(RefreshOutcome::Refreshed | RefreshOutcome::Disconnected) => {}
            Ok(RefreshOutcome::Unreachable(reason)) => {
                return render(
                    tier,
                    StatusResponse::Unreachable {
                        host_id: host.as_str().to_owned(),
                        reason: reason.as_str(),
                        agents: agent_rows(&visible.agents, None),
                        last_seen_at: computer.last_seen_at,
                    },
                );
            }
            Err(cause) => {
                tracing::error!(
                    error = crate::foundation::safe_log_field::safe_display(&(cause)),
                    "host status refresh failed"
                );
                return internal_error();
            }
        }
    }

    match state.seeds.read(&host) {
        Ok(SeedRead::Disconnected) => render(
            tier,
            StatusResponse::Disconnected {
                host_id: host.as_str().to_owned(),
                agents: agent_rows(&visible.agents, None),
                last_seen_at: computer.last_seen_at,
                admission_failure: None,
            },
        ),
        Ok(SeedRead::NotSeeded) => render(
            tier,
            StatusResponse::Unreachable {
                host_id: host.as_str().to_owned(),
                reason: "daemon_unreachable",
                agents: agent_rows(&visible.agents, None),
                last_seen_at: computer.last_seen_at,
            },
        ),
        Ok(SeedRead::Seeded(seed)) => {
            available(tier, &host, &visible.agents, computer.last_seen_at, seed)
        }
        Err(cause) => {
            tracing::error!(
                error = crate::foundation::safe_log_field::safe_display(&(cause)),
                "host status seed read failed"
            );
            internal_error()
        }
    }
}

fn available(
    tier: ReadTier,
    host: &HostId,
    agents: &[&HostStatusAgentFact],
    last_seen_at: Option<i64>,
    seed: SeededStatus,
) -> Response {
    if ![seed.daemon_pid, seed.started_at, seed.uptime_ms]
        .into_iter()
        .all(js_safe_integer)
    {
        tracing::error!(
            host_id = crate::foundation::safe_log_field::safe_display(&(host.as_str())),
            "host status snapshot is not JS-safe"
        );
        return internal_error();
    }
    render(
        tier,
        StatusResponse::Available {
            host_id: host.as_str().to_owned(),
            snapshot: Snapshot {
                os: seed.os,
                os_arch: seed.os_arch,
                daemon_version: seed.daemon_version,
                daemon_pid: seed.daemon_pid,
                started_at: seed.started_at,
                uptime_ms: seed.uptime_ms,
                hostname: seed.machine.hostname,
                ip: seed.machine.ip,
                memory_total_bytes: seed.machine.memory.map(|usage| usage.total_bytes),
                memory_used_bytes: seed.machine.memory.map(|usage| usage.used_bytes),
                disk_total_bytes: seed.machine.disk.map(|usage: Usage| usage.total_bytes),
                disk_used_bytes: seed.machine.disk.map(|usage| usage.used_bytes),
                system_uptime_ms: seed
                    .machine
                    .system_uptime_ms_at_answer
                    .filter(|value| js_safe_integer(*value)),
            },
            agents: agent_rows(agents, Some(&seed.active_agents)),
            last_seen_at,
        },
    )
}

fn agent_rows(
    agents: &[&HostStatusAgentFact],
    active: Option<&HashSet<AgentId>>,
) -> Vec<AgentOnHost> {
    agents
        .iter()
        .filter(|agent| agent.materialization_status != MaterializationStatus::Failed)
        .map(|agent| AgentOnHost {
            agent_id: agent.id.as_str().to_owned(),
            display_name: agent.display_name.clone(),
            handle: format!("@{}", agent.name),
            runtime: agent.runtime.as_str(),
            model: agent.model.clone(),
            reasoning_effort: agent.reasoning_effort.clone(),
            fast_mode: agent.fast_mode,
            avatar_url: agent
                .avatar_storage_key
                .as_deref()
                .filter(|key| !key.is_empty())
                .map(|_| {
                    format!(
                        "/api/agents/{}/avatar?v={}",
                        agent.id.as_str(),
                        agent.updated_at
                    )
                }),
            online: if active.is_some_and(|set| set.contains(&agent.id)) {
                "online"
            } else {
                "offline"
            },
        })
        .collect()
}

fn render(tier: ReadTier, body: StatusResponse) -> Response {
    match tier {
        ReadTier::Privileged => Json(Tiered {
            view: tier.as_str(),
            body,
        })
        .into_response(),
        ReadTier::Public => Json(Tiered {
            view: tier.as_str(),
            body: PublicStatusResponse::from(body),
        })
        .into_response(),
    }
}

#[derive(Serialize)]
struct Tiered<T> {
    view: &'static str,
    #[serde(flatten)]
    body: T,
}

/// The public tier: the same arms without machine detail, the unreachable reason,
/// or the admission failure. A separate type so a new privileged field cannot leak
/// by default.
#[derive(Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
enum PublicStatusResponse {
    Available {
        host_id: String,
        snapshot: PublicSnapshot,
        agents: Vec<AgentOnHost>,
        last_seen_at: Option<i64>,
    },
    Disconnected {
        host_id: String,
        agents: Vec<AgentOnHost>,
        last_seen_at: Option<i64>,
    },
    Unreachable {
        host_id: String,
        agents: Vec<AgentOnHost>,
        last_seen_at: Option<i64>,
    },
    Revoked {
        host_id: String,
        agents: Vec<AgentOnHost>,
        last_seen_at: Option<i64>,
    },
}

#[derive(Serialize)]
struct PublicSnapshot {
    os: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_arch: Option<String>,
}

impl From<StatusResponse> for PublicStatusResponse {
    fn from(body: StatusResponse) -> Self {
        match body {
            StatusResponse::Available {
                host_id,
                snapshot,
                agents,
                last_seen_at,
            } => Self::Available {
                host_id,
                snapshot: PublicSnapshot {
                    os: snapshot.os,
                    os_arch: snapshot.os_arch,
                },
                agents,
                last_seen_at,
            },
            StatusResponse::Disconnected {
                host_id,
                agents,
                last_seen_at,
                admission_failure: _,
            } => Self::Disconnected {
                host_id,
                agents,
                last_seen_at,
            },
            StatusResponse::Unreachable {
                host_id,
                reason: _,
                agents,
                last_seen_at,
            } => Self::Unreachable {
                host_id,
                agents,
                last_seen_at,
            },
            StatusResponse::Revoked {
                host_id,
                agents,
                last_seen_at,
            } => Self::Revoked {
                host_id,
                agents,
                last_seen_at,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
enum StatusResponse {
    Available {
        host_id: String,
        snapshot: Snapshot,
        agents: Vec<AgentOnHost>,
        last_seen_at: Option<i64>,
    },
    Disconnected {
        host_id: String,
        agents: Vec<AgentOnHost>,
        last_seen_at: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        admission_failure: Option<AdmissionFailure>,
    },
    Unreachable {
        host_id: String,
        reason: &'static str,
        agents: Vec<AgentOnHost>,
        last_seen_at: Option<i64>,
    },
    Revoked {
        host_id: String,
        agents: Vec<AgentOnHost>,
        last_seen_at: Option<i64>,
    },
}

#[derive(Serialize)]
struct Snapshot {
    os: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_arch: Option<String>,
    daemon_version: String,
    daemon_pid: i64,
    started_at: i64,
    uptime_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_total_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_used_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_total_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_used_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_uptime_ms: Option<i64>,
}

#[derive(Serialize)]
struct AgentOnHost {
    agent_id: String,
    display_name: String,
    handle: String,
    runtime: &'static str,
    model: Option<String>,
    reasoning_effort: Option<String>,
    fast_mode: Option<bool>,
    avatar_url: Option<String>,
    online: &'static str,
}

#[derive(Serialize)]
struct AdmissionFailure {
    kind: &'static str,
    offered_protocol_version: u32,
    expected_protocol_version: u32,
    observed_at: i64,
}

fn not_found() -> Response {
    (
        axum::http::StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "host not found", "error_code": "host_not_found" })),
    )
        .into_response()
}
