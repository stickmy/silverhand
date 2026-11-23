//! Runtime tuning admission. Effort values belong to each runtime's CLI, so writes
//! check only the token shape and whether the runtime takes an effort at all.

use crate::identity_access::agents::runtime::RuntimeKind;

/// Upper bound of `ReasoningEffortTokenSchema`.
pub const REASONING_EFFORT_TOKEN_MAX: usize = 32;

/// `ReasoningEffortTokenSchema`: `^[a-z][a-z0-9_-]*$`, 1..=32 chars.
pub fn is_reasoning_effort_token(value: &str) -> bool {
    value.len() <= REASONING_EFFORT_TOKEN_MAX
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

/// Mirrors `runtimeSupportsReasoningEffort`: the driver forwards an effort.
pub fn runtime_supports_reasoning_effort(kind: RuntimeKind) -> bool {
    match kind {
        RuntimeKind::Claude
        | RuntimeKind::Codex
        | RuntimeKind::Qoder
        | RuntimeKind::Pi
        | RuntimeKind::Grok => true,
        RuntimeKind::Opencode
        | RuntimeKind::Cursor
        | RuntimeKind::Gemini
        | RuntimeKind::Antigravity => false,
    }
}

/// Null delegates to the runtime default. Whether the CLI accepts the value for the
/// chosen model is the runtime's call, surfaced when it runs.
pub fn reasoning_effort_valid_for_runtime(kind: RuntimeKind, value: Option<&str>) -> bool {
    value.is_none_or(|value| {
        runtime_supports_reasoning_effort(kind) && is_reasoning_effort_token(value)
    })
}

/// Whether the runtime takes a service tier. Only Codex does; mirrors
/// `runtimeSupportsServiceTier`.
pub fn runtime_supports_service_tier(kind: RuntimeKind) -> bool {
    matches!(kind, RuntimeKind::Codex)
}

/// Tier ids (`default`, `priority`, `ultrafast`, ...) belong to the CLI, so only the
/// token shape is checked; null delegates to the CLI default.
pub fn service_tier_valid_for_runtime(kind: RuntimeKind, value: Option<&str>) -> bool {
    value
        .is_none_or(|value| runtime_supports_service_tier(kind) && is_reasoning_effort_token(value))
}

/// The model a runtime FORCES at create time when the request omits one (or gives a
/// blank one), or `None` when the runtime has no forced create-time default.
///
/// Mirrors `runtimeDefaultModel` (`CATALOG[kind].defaultModel ?? null`): today the
/// only entry carrying a `defaultModel` is Qoder (`'Ultimate'`); Codex and Claude
/// deliberately carry none (free-text / driver-default), and the roadmap runtimes
/// carry none either. This is the sole default applied by the create write path, so a
/// direct-API/CLI create agrees with a UI create on the persisted `model` column.
pub fn runtime_default_model(kind: RuntimeKind) -> Option<&'static str> {
    match kind {
        RuntimeKind::Qoder => Some("Ultimate"),
        RuntimeKind::Claude
        | RuntimeKind::Codex
        | RuntimeKind::Opencode
        | RuntimeKind::Cursor
        | RuntimeKind::Gemini
        | RuntimeKind::Pi
        | RuntimeKind::Grok
        | RuntimeKind::Antigravity => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_without_a_reasoning_control_rejects_every_non_null_effort() {
        for kind in [
            RuntimeKind::Opencode,
            RuntimeKind::Cursor,
            RuntimeKind::Gemini,
            RuntimeKind::Antigravity,
        ] {
            assert!(reasoning_effort_valid_for_runtime(kind, None));
            for token in ["low", "high", "max", "none"] {
                assert!(!reasoning_effort_valid_for_runtime(kind, Some(token)));
            }
        }
    }

    #[test]
    fn effort_admission_is_structural_for_every_reasoning_runtime() {
        for kind in [
            RuntimeKind::Claude,
            RuntimeKind::Codex,
            RuntimeKind::Qoder,
            RuntimeKind::Pi,
            RuntimeKind::Grok,
        ] {
            for token in ["low", "max", "ultra", "future-tier", "none", "a_1"] {
                assert!(reasoning_effort_valid_for_runtime(kind, Some(token)));
            }
            for token in [
                "",
                "High",
                "high;exec",
                "a b",
                "9tier",
                "超高",
                "123456789012345678901234567890123",
            ] {
                assert!(!reasoning_effort_valid_for_runtime(kind, Some(token)));
            }
        }
    }

    #[test]
    fn only_qoder_forces_a_create_time_default_model() {
        assert_eq!(runtime_default_model(RuntimeKind::Qoder), Some("Ultimate"));
        // Every other runtime — including the free-text ones (Claude/Codex) and the
        // roadmap runtimes — forces nothing, so a create request that omits `model`
        // persists a NULL model, not a fabricated default.
        for kind in [
            RuntimeKind::Claude,
            RuntimeKind::Codex,
            RuntimeKind::Opencode,
            RuntimeKind::Cursor,
            RuntimeKind::Gemini,
            RuntimeKind::Pi,
            RuntimeKind::Grok,
        ] {
            assert_eq!(runtime_default_model(kind), None, "{kind:?}");
        }
    }

    #[test]
    fn only_codex_supports_service_tier() {
        assert!(runtime_supports_service_tier(RuntimeKind::Codex));
        for kind in [
            RuntimeKind::Claude,
            RuntimeKind::Qoder,
            RuntimeKind::Opencode,
            RuntimeKind::Cursor,
            RuntimeKind::Gemini,
            RuntimeKind::Pi,
            RuntimeKind::Grok,
        ] {
            assert!(!runtime_supports_service_tier(kind), "{kind:?}");
        }
    }
}
