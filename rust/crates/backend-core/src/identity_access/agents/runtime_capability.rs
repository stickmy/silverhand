//! Runtime tuning admission. Claude/Codex own model-specific effort support;
//! we validate token shape only. Other runtimes retain their curated options.

use crate::identity_access::agents::runtime::RuntimeKind;

fn baseline_reasoning_effort_options(kind: RuntimeKind) -> &'static [&'static str] {
    match kind {
        // Native runtimes bypass this table; the others expose no reasoning control.
        RuntimeKind::Claude
        | RuntimeKind::Codex
        | RuntimeKind::Opencode
        | RuntimeKind::Cursor
        | RuntimeKind::Gemini => &[],
        RuntimeKind::Grok => &["low", "medium", "high"],
        RuntimeKind::Pi => &["off", "minimal", "low", "medium", "high"],
        RuntimeKind::Qoder => &["none", "low", "medium", "high", "xhigh", "max"],
    }
}

fn per_model_reasoning_effort_options(
    kind: RuntimeKind,
    model_id: &str,
) -> Option<&'static [&'static str]> {
    match kind {
        RuntimeKind::Grok => match model_id {
            "grok-4.6" => Some(&["low", "medium", "high", "xhigh"]),
            _ => None,
        },
        // Remaining runtimes use the baseline.
        RuntimeKind::Claude
        | RuntimeKind::Codex
        | RuntimeKind::Qoder
        | RuntimeKind::Opencode
        | RuntimeKind::Cursor
        | RuntimeKind::Gemini
        | RuntimeKind::Pi => None,
    }
}

pub fn reasoning_effort_options_for_runtime(
    kind: RuntimeKind,
    model: Option<&str>,
) -> &'static [&'static str] {
    let model_id = model.map(str::trim).filter(|id| !id.is_empty());
    if let Some(model_id) = model_id
        && let Some(options) = per_model_reasoning_effort_options(kind, model_id)
    {
        return options;
    }
    baseline_reasoning_effort_options(kind)
}

/// Null delegates to the runtime default. Native model catalogs never gate writes.
pub fn reasoning_effort_valid_for_runtime(
    kind: RuntimeKind,
    value: Option<&str>,
    model: Option<&str>,
) -> bool {
    match value {
        None => true,
        Some(value) if matches!(kind, RuntimeKind::Claude | RuntimeKind::Codex) => {
            value.len() <= 32
                && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && value
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
        }
        Some(value) => reasoning_effort_options_for_runtime(kind, model).contains(&value),
    }
}

/// Whether the runtime accepts the persisted fast-mode tri-state. Only Codex does;
/// mirrors `runtimeSupportsFastMode` (the sole `supportsFastMode: true` entry).
pub fn runtime_supports_fast_mode(kind: RuntimeKind) -> bool {
    matches!(kind, RuntimeKind::Codex)
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
        | RuntimeKind::Grok => None,
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
        ] {
            assert!(reasoning_effort_options_for_runtime(kind, None).is_empty());
            // Unset is always legal …
            assert!(reasoning_effort_valid_for_runtime(kind, None, None));
            // … but any concrete token — even one legal elsewhere — is out of range.
            for token in ["low", "medium", "high", "xhigh", "max", "ultra", "none"] {
                assert!(
                    !reasoning_effort_valid_for_runtime(kind, Some(token), None),
                    "{kind:?} must reject {token:?}"
                );
            }
        }
    }

    #[test]
    fn native_runtime_effort_admission_is_structural_not_a_model_allowlist() {
        for kind in [RuntimeKind::Claude, RuntimeKind::Codex] {
            for model in [None, Some("custom/future-model"), Some("gpt-5.6-terra")] {
                for token in ["low", "max", "ultra", "future-tier", "none"] {
                    assert!(reasoning_effort_valid_for_runtime(kind, Some(token), model));
                }
                for token in [
                    "",
                    "High",
                    "high;exec",
                    "a b",
                    "9tier",
                    "超高",
                    "12345678901234567890123456789012345",
                ] {
                    assert!(!reasoning_effort_valid_for_runtime(
                        kind,
                        Some(token),
                        model
                    ));
                }
            }
        }
    }

    #[test]
    fn qoder_accepts_its_full_set_including_none() {
        for token in ["none", "low", "medium", "high", "xhigh", "max"] {
            assert!(reasoning_effort_valid_for_runtime(
                RuntimeKind::Qoder,
                Some(token),
                None
            ));
        }
        // qoder has no per-model override, so a model never widens or narrows it.
        assert!(reasoning_effort_valid_for_runtime(
            RuntimeKind::Qoder,
            Some("none"),
            Some("Ultimate")
        ));
        // `ultra` is codex's, not qoder's.
        assert!(!reasoning_effort_valid_for_runtime(
            RuntimeKind::Qoder,
            Some("ultra"),
            None
        ));
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
    fn only_codex_supports_fast_mode() {
        assert!(runtime_supports_fast_mode(RuntimeKind::Codex));
        for kind in [
            RuntimeKind::Claude,
            RuntimeKind::Qoder,
            RuntimeKind::Opencode,
            RuntimeKind::Cursor,
            RuntimeKind::Gemini,
            RuntimeKind::Pi,
            RuntimeKind::Grok,
        ] {
            assert!(!runtime_supports_fast_mode(kind), "{kind:?}");
        }
    }
}
