#![forbid(unsafe_code)]
#![deny(rustdoc::broken_intra_doc_links)]
//! HTTP, WebSocket and wire projection boundary.

#[cfg(test)]
#[macro_use]
#[path = "../../backend-sqlite/tests/support/seed_rows.rs"]
mod seed_rows;

pub mod application_orchestration;
pub mod conversation_messaging;
pub mod embedded_bootstrap;
pub mod foundation;
pub mod host_runtime;
pub mod identity_access;
pub mod scheduling;
pub mod viewer_read_model;
pub mod workspace_data_plane;

#[cfg(test)]
#[path = "../../backend-sqlite/tests/support/current_schema_snapshot.rs"]
mod current_schema;
