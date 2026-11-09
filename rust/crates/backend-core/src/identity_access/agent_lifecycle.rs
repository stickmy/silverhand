//! Which stage of materialization an agent row is in.
//!
//! A sibling-shared vocabulary: visibility widens on it, teardown dispatches on
//! it, and a cold read projects it. It therefore cannot live inside any one of
//! those modules without making the other two depend on that module for a fact
//! it does not own.

use thiserror::Error;

/// The closed set of materialization stages. Closed because every consumer
/// branches exhaustively on it: a new stage must state its visibility, its
/// teardown obligation and its projection at the same time.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MaterializationStatus {
    PendingMaterialization,
    Active,
    Failed,
}

impl MaterializationStatus {
    /// The stable persisted token. Adapters read and write exactly these.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingMaterialization => "pending_materialization",
            Self::Active => "active",
            Self::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, UnknownMaterializationStatus> {
        match raw {
            "pending_materialization" => Ok(Self::PendingMaterialization),
            "active" => Ok(Self::Active),
            "failed" => Ok(Self::Failed),
            _ => Err(UnknownMaterializationStatus),
        }
    }
}

/// A stored value outside the closed set is unreadable, not a fourth stage: it
/// reaches the caller as an error and never as a decision input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("materialization status is outside the closed set")]
pub struct UnknownMaterializationStatus;

/// The stored facts an agent's lifecycle is derived from, and only those.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentLifecycleStamps {
    pub materialization: MaterializationStatus,
    pub deletion_started_at: Option<i64>,
    /// Whether a runtime transition lock is held (target and started-at are paired by
    /// schema trigger).
    pub transition_held: bool,
}

/// Where an agent is in its life. Derived, never stored, so every gate reads the same
/// answer from the same stamps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentLifecycle {
    Pending,
    Failed,
    Active,
    Transitioning,
    /// Carries the deletion marker, which is also the teardown attempt's token.
    Deleting {
        started_at: i64,
    },
}

/// Deletion is terminal and outranks everything; a stage other than active outranks a
/// transition lock.
pub fn agent_lifecycle(stamps: &AgentLifecycleStamps) -> AgentLifecycle {
    if let Some(started_at) = stamps.deletion_started_at {
        return AgentLifecycle::Deleting { started_at };
    }
    match stamps.materialization {
        MaterializationStatus::PendingMaterialization => AgentLifecycle::Pending,
        MaterializationStatus::Failed => AgentLifecycle::Failed,
        MaterializationStatus::Active if stamps.transition_held => AgentLifecycle::Transitioning,
        MaterializationStatus::Active => AgentLifecycle::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_precedence_is_deletion_then_stage_then_transition() {
        let stamps = |materialization, deletion_started_at, transition_held| AgentLifecycleStamps {
            materialization,
            deletion_started_at,
            transition_held,
        };
        use MaterializationStatus::*;
        for (input, expected) in [
            (stamps(Active, None, false), AgentLifecycle::Active),
            (stamps(Active, None, true), AgentLifecycle::Transitioning),
            (
                stamps(Active, Some(1), true),
                AgentLifecycle::Deleting { started_at: 1 },
            ),
            (
                stamps(PendingMaterialization, None, true),
                AgentLifecycle::Pending,
            ),
            (stamps(Failed, None, false), AgentLifecycle::Failed),
            (
                stamps(Failed, Some(2), false),
                AgentLifecycle::Deleting { started_at: 2 },
            ),
        ] {
            assert_eq!(agent_lifecycle(&input), expected, "{input:?}");
        }
    }

    #[test]
    fn every_stage_uses_its_stable_persisted_token() {
        for (status, token) in [
            (
                MaterializationStatus::PendingMaterialization,
                "pending_materialization",
            ),
            (MaterializationStatus::Active, "active"),
            (MaterializationStatus::Failed, "failed"),
        ] {
            assert_eq!(status.as_str(), token);
            assert_eq!(MaterializationStatus::parse(token), Ok(status));
        }
    }

    #[test]
    fn an_unknown_token_is_unreadable_rather_than_a_fourth_stage() {
        assert_eq!(
            MaterializationStatus::parse("archived"),
            Err(UnknownMaterializationStatus)
        );
        assert_eq!(
            MaterializationStatus::parse(""),
            Err(UnknownMaterializationStatus)
        );
    }
}
