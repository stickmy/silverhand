#![forbid(unsafe_code)]
#![deny(rustdoc::broken_intra_doc_links)]
//! SQLite adapters for `backend-core`'s ports.
//!
//! Everything specific to the engine stays inside this crate: SQL text, schema,
//! PRAGMAs, transaction modes and driver errors. The ports return application
//! facts and `PersistenceFailure`, so no driver type reaches the application or
//! the wire.

#[cfg(test)]
#[macro_use]
#[path = "../tests/support/seed_rows.rs"]
mod seed_rows;

pub mod application_orchestration;
pub mod conversation_messaging;
pub mod embedded_bootstrap;
pub mod foundation;
pub mod host_runtime;
pub mod identity_access;
pub mod viewer_read_model;
pub mod workspace_data_plane;

#[cfg(test)]
#[path = "../tests/support/current_schema_snapshot.rs"]
mod current_schema;
