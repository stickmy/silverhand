//! `foundation` — a dependency-substrate namespace container for backend-sqlite.
//!
//! A namespace container, not a bounded context or a shared lifecycle: it groups the
//! backend-sqlite crate roots the organization map assigns to owner `foundation` under a
//! single Rust module path (`crate::foundation::*`). It declares its members and holds no
//! SQL, policy, transaction, schema, clock, or lifecycle logic.
//!
//! Colocation does NOT merge the members: the shared host-agent connection substrate
//! (`host_agent_store`), scope-authority adapter (`scope_event`), and write-readiness
//! transport (`write_probe`) remain independent. Their pools, errors, SQL, traits,
//! factories, and lifecycles are distinct; the host-agent store is shared only by the
//! typed owner adapters that implement ports on it. The cfg-test `current_schema`
//! snapshot the map also assigns to owner `foundation` is a separate axis: it stays a
//! crate-root `#[cfg(test)]`/`#[path]` declaration and is NOT physically colocated here.

pub mod database;
pub mod database_startup;
pub mod host_agent_store;
pub mod scope_event;
pub mod write_probe;

pub(crate) mod statement_metrics;
