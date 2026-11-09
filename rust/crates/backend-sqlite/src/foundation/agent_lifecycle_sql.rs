//! The SQL spelling of the agent lifecycle derivation
//! ([`agent_lifecycle`](silverhand_backend_core::identity_access::agent_lifecycle::agent_lifecycle)).
//!
//! Each gate composes the stamps it needs from these fragments so no statement spells a
//! lifecycle stamp by hand. `$p` is the table prefix (`""` or `"a."`). The transition
//! pair is held together by schema trigger, so the target alone decides "held".

macro_rules! agent_materialized {
    ($p:literal) => {
        concat!($p, "materialization_status = 'active'")
    };
}

macro_rules! agent_not_deleting {
    ($p:literal) => {
        concat!($p, "deletion_started_at IS NULL")
    };
}

macro_rules! agent_transition_clear {
    ($p:literal) => {
        concat!($p, "runtime_transition_target IS NULL")
    };
}

/// `AgentLifecycle::Active`: materialized, not deleting, no transition held.
macro_rules! agent_active {
    ($p:literal) => {
        concat!(
            $crate::foundation::agent_lifecycle_sql::agent_materialized!($p),
            " AND ",
            $crate::foundation::agent_lifecycle_sql::agent_not_deleting!($p),
            " AND ",
            $crate::foundation::agent_lifecycle_sql::agent_transition_clear!($p)
        )
    };
}

pub(crate) use {agent_active, agent_materialized, agent_not_deleting, agent_transition_clear};
