//! The internal agent task-WRITE faces (`POST /internal/agents/{aid}/tasks/{create,
//! claim,unclaim,update-status,edit}`) — the thin adapters that authorize a write at
//! the door and hand it to the shared task-write unit of work.
//!
//! Each face binds the acting owner to the path `{aid}` (the shared
//! `bind_owner`-style guard), derives the runtime scope (agent-slot vs
//! conversation-frame) from the credential, resolves the by-number board via
//! [`selector`] (or takes the by-id arm, agent-slot only), gates the face's
//! capability token, and dispatches to the core write UoW. No face re-implements
//! the state machine or SQL: the five UoWs are the exact denominator.

pub(crate) mod assign;
pub(crate) mod claim;
pub(crate) mod create;
pub(crate) mod edit;
pub(crate) mod selector;
pub(crate) mod unclaim;
pub(crate) mod update_status;
