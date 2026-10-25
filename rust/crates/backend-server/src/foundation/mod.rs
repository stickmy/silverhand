//! Shared technical substrate, wire vocabulary, and process-wide transport/protocol
//! foundations.
//!
//! Bearer extraction, process time, and multipart streaming remain independent technical
//! leaves; colocation here does not merge their contracts or responsibilities.
//! Error-chain reporting, JSON object admission, and PATCH presence algebra likewise remain
//! independent framework leaves.
//! CLI invoke observation is an adapter around already-composed routes, while CLI preflight trace
//! is its own authenticated HTTP face; neither owns the runtime-credential authority it reads.
//! Bind-address scope and deployment profile are closed process vocabularies. Process health,
//! process readiness, and server info are three independent HTTP verticals: health remains
//! dependency-blind, readiness alone owns the live boot-progress holder and write probe, and
//! server info advertises immutable build/configuration facts.

pub(crate) mod bearer;
pub mod bind_address_scope;
pub mod cli_invoke_observer;
pub mod cli_preflight_trace;
pub mod clock;
pub(crate) mod daemon_command;
pub(crate) mod daemon_transport;
pub mod deployment_profile;
pub(crate) mod error;
pub(crate) mod http_error;
pub(crate) mod multipart_transport;
pub mod operator_console;
pub mod process_health;
pub mod process_readiness;
pub mod safe_log_field;
pub mod scope_event_transport;
pub(crate) mod scope_event_wire;
pub mod server_info;
pub(crate) mod wire_admission;
pub(crate) mod wire_coerce;
pub(crate) mod wire_patch;

mod daemon_metrics;
