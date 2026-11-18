//! Typed `host_proxy_config_get` / `host_proxy_config_set` command contracts.
//!
//! Proxy bytes remain daemon-host local truth. This leaf only binds a request and
//! answer to one `(host, daemon instance)`, validates the shared DTO, and maps the
//! transport's closed failures. It owns no file, cache or fallback store.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use silverhand_backend_core::identity_access::agents::runtime::RuntimeKind;

use crate::application_orchestration::daemon_delivery::{
    HostDataPlaneCustody, HostDataPlaneFailure, HostDataPlaneRequest, OwningHostDataPlane, command,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProxyConfig {
    pub proxy_url: Option<String>,
    #[serde(serialize_with = "serialize_runtimes")]
    pub proxy_runtimes: Vec<RuntimeKind>,
}

pub struct ProxyConfigRequest {
    custody: HostDataPlaneCustody,
}

impl ProxyConfigRequest {
    pub fn new(custody: HostDataPlaneCustody) -> Self {
        Self { custody }
    }
}

pub enum ProxyConfigOutcome {
    Answered(ProxyConfig),
    AttachPending,
    Timeout,
    DaemonInternalError,
    TransportContractViolation,
}

pub async fn get(
    dispatch: &OwningHostDataPlane,
    request: ProxyConfigRequest,
) -> ProxyConfigOutcome {
    round_trip(dispatch, request, command::proxy_config_get(json!({}))).await
}

pub async fn set(
    dispatch: &OwningHostDataPlane,
    request: ProxyConfigRequest,
    config: ProxyConfig,
) -> ProxyConfigOutcome {
    let params = json!({
        "proxy_url": config.proxy_url,
        "proxy_runtimes": config
            .proxy_runtimes
            .iter()
            .map(RuntimeKind::as_str)
            .collect::<Vec<_>>(),
    });
    round_trip(dispatch, request, command::proxy_config_set(params)).await
}

async fn round_trip(
    dispatch: &OwningHostDataPlane,
    request: ProxyConfigRequest,
    command: command::EncodedDaemonCommand,
) -> ProxyConfigOutcome {
    let answer = match dispatch
        .dispatch(HostDataPlaneRequest {
            custody: request.custody.clone(),
            command,
            timeout: COMMAND_TIMEOUT,
        })
        .await
    {
        Ok(answer) => answer,
        Err(failure) => return outcome_for(failure),
    };
    let Ok(result) = answer.into_result_for(&request.custody) else {
        return ProxyConfigOutcome::TransportContractViolation;
    };
    match ProxyConfigResultWire::decode(result) {
        Some(config) => ProxyConfigOutcome::Answered(config),
        None => ProxyConfigOutcome::DaemonInternalError,
    }
}

fn outcome_for(failure: HostDataPlaneFailure) -> ProxyConfigOutcome {
    match failure {
        HostDataPlaneFailure::NoCurrentDaemon
        | HostDataPlaneFailure::ConnectionGenerationMismatch
        | HostDataPlaneFailure::DaemonInstanceMismatch
        | HostDataPlaneFailure::NotDispatched => ProxyConfigOutcome::AttachPending,
        HostDataPlaneFailure::TimedOut => ProxyConfigOutcome::Timeout,
        HostDataPlaneFailure::Refused(_)
        | HostDataPlaneFailure::Ambiguous
        | HostDataPlaneFailure::ProtocolCorruption
        | HostDataPlaneFailure::RegistryUnavailable => ProxyConfigOutcome::DaemonInternalError,
    }
}

#[derive(Deserialize)]
struct ProxyConfigResultWire {
    config: ProxyConfigWire,
}

#[derive(Deserialize)]
struct ProxyConfigWire {
    proxy_url: Option<String>,
    proxy_runtimes: Vec<String>,
}

impl ProxyConfigResultWire {
    fn decode(value: Value) -> Option<ProxyConfig> {
        let wire = serde_json::from_value::<Self>(value).ok()?;
        Some(ProxyConfig {
            proxy_url: wire.config.proxy_url,
            proxy_runtimes: wire
                .config
                .proxy_runtimes
                .into_iter()
                .map(|raw| RuntimeKind::parse(&raw))
                .collect::<Option<Vec<_>>>()?,
        })
    }
}

fn serialize_runtimes<S>(runtimes: &[RuntimeKind], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    runtimes
        .iter()
        .map(RuntimeKind::as_str)
        .collect::<Vec<_>>()
        .serialize(serializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_uses_the_shared_closed_runtime_set_and_strips_nested_extras() {
        let config = ProxyConfigResultWire::decode(json!({
            "config": {
                "proxy_url": "http://127.0.0.1:7890",
                "proxy_runtimes": ["codex", "qoder"],
                "newer_daemon_field": true
            }
        }))
        .unwrap();
        assert_eq!(config.proxy_url.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(
            config.proxy_runtimes,
            vec![RuntimeKind::Codex, RuntimeKind::Qoder]
        );
        assert!(
            ProxyConfigResultWire::decode(json!({
                "config": { "proxy_url": null, "proxy_runtimes": ["banana"] }
            }))
            .is_none()
        );
    }

    #[test]
    fn pending_and_timeout_transport_failures_have_distinct_outcomes() {
        for pending in [
            HostDataPlaneFailure::NoCurrentDaemon,
            HostDataPlaneFailure::ConnectionGenerationMismatch,
            HostDataPlaneFailure::DaemonInstanceMismatch,
            HostDataPlaneFailure::NotDispatched,
        ] {
            assert!(matches!(
                outcome_for(pending),
                ProxyConfigOutcome::AttachPending
            ));
        }
        assert!(matches!(
            outcome_for(HostDataPlaneFailure::TimedOut),
            ProxyConfigOutcome::Timeout
        ));
    }
}
