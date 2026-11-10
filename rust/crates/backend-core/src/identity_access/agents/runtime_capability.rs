//! Which reasoning-effort tokens a runtime/model pair accepts, and whether a
//! runtime exposes fast-mode — the legality every agents-row write enforces.
//!
//! ## A capability layer over the closed runtime set, distinct from detection
//!
//! [`RuntimeKind`] owns what a runtime IS; [`super::runtime_catalog`] owns which
//! runtimes a machine OFFERS and what a probe found. This module owns a third,
//! separate product decision: given a runtime (and, for reasoning, a model), which
//! stored `reasoning_effort` tokens are legal, and does the runtime accept the
//! persisted fast-mode tri-state. It moves for its own reason — a runtime gaining an
//! effort tier, or a newly-verified model exposing `max`/`ultra` — so it is neither
//! the vocabulary nor the offered catalogue.
//!
//! ## The AUTHORITATIVE allowlist, shared by every write boundary
//!
//! These predicates are the sole legality the create, runtime-switch, and profile
//! PATCH paths enforce, mirroring `packages/shared/src/runtime-catalog.ts`
//! (`reasoningEffortValidForRuntime` / `runtimeSupportsFastMode` and the
//! `reasoningEffortOptionsForRuntime` they read). Keeping the allowed set here — the
//! ONE place — is what stops a value legal for one runtime/model (qoder's `none`,
//! Codex's model-specific `ultra`) from leaking onto another: a direct-API write or a
//! runtime switch can't persist an out-of-range token that would then be forwarded to
//! the driver.
//!
//! The tokens are compared as raw strings, exactly as they are stored and passed to
//! the driver. Format (`^[a-z][a-z0-9_-]*$`, 1..=32) is a separate admission concern
//! settled at the wire; this module answers only membership.
//!
//! ## The per-model overrides are an exact-id table, never a prefix match
//!
//! Codex `max`/`ultra` use exact verified 5.6 model ids and an approved GPT-6 family. A future
//! `gpt-5.6-*` id is deliberately NOT matched by prefix — it stays on the
//! conservative runtime baseline until an explicit entry verifies it, matching the
//! TypeScript comment that refuses to grant `max` to an unverified id.

use crate::identity_access::agents::runtime::RuntimeKind;

/// The runtime baseline reasoning-effort options, in display order — the set a
/// runtime accepts for any model without a verified per-model override. Empty means
/// the runtime exposes no reasoning control at all, so any non-null effort is
/// illegal. Pinned to the `reasoningEffortOptions` of each catalog entry.
fn baseline_reasoning_effort_options(kind: RuntimeKind) -> &'static [&'static str] {
    match kind {
        // No reasoning control: any stored effort is out of range.
        RuntimeKind::Claude | RuntimeKind::Opencode | RuntimeKind::Cursor | RuntimeKind::Gemini => {
            &[]
        }
        RuntimeKind::Grok => &["low", "medium", "high"],
        RuntimeKind::Pi => &["off", "minimal", "low", "medium", "high"],
        RuntimeKind::Codex => &["low", "medium", "high", "xhigh"],
        RuntimeKind::Qoder => &["none", "low", "medium", "high", "xhigh", "max"],
    }
}

/// A verified model's FULL replacement option list, or `None` to fall back to the
/// runtime baseline. The exact-id table mirrors `reasoningEffortOptionsByModel`:
/// all `gpt-5.6` models expose `max`; only `sol` additionally exposes `ultra`. An
/// unknown, custom, or driver-default model returns `None` and stays on the
/// conservative baseline. GPT-6 additionally has an approved, hyphen-delimited
/// family rule matching the shared catalog; 5.6 remains exact-id only.
fn per_model_reasoning_effort_options(
    kind: RuntimeKind,
    model_id: &str,
) -> Option<&'static [&'static str]> {
    match kind {
        RuntimeKind::Codex => match model_id {
            id if id == "gpt-6"
                || id
                    .strip_prefix("gpt-6-")
                    .is_some_and(|suffix| !suffix.is_empty()) =>
            {
                Some(&["low", "medium", "high", "xhigh", "max", "ultra"])
            }
            "gpt-6.1-sol" | "gpt-6.1-terra" | "gpt-5.6-sol" => {
                Some(&["low", "medium", "high", "xhigh", "max", "ultra"])
            }
            "gpt-5.6-terra" | "gpt-5.6-luna" => Some(&["low", "medium", "high", "xhigh", "max"]),
            _ => None,
        },
        RuntimeKind::Grok => match model_id {
            "grok-4.6" => Some(&["low", "medium", "high", "xhigh"]),
            _ => None,
        },
        // Remaining runtimes use the baseline.
        RuntimeKind::Claude
        | RuntimeKind::Qoder
        | RuntimeKind::Opencode
        | RuntimeKind::Cursor
        | RuntimeKind::Gemini
        | RuntimeKind::Pi => None,
    }
}

/// The reasoning-effort tokens legal for `kind` at `model`, model-aware.
///
/// The runtime baseline is the safe fallback for the driver default (`None`), a
/// blank model, a custom id, and any future model whose capability has not been
/// explicitly verified. A verified model's override REPLACES the baseline whole
/// (it is not additive). Mirrors `reasoningEffortOptionsForRuntime`, including its
/// `model?.trim()` — a whitespace-only model is treated as absent.
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

/// Whether a stored `reasoning_effort` is legal for a runtime/model pair.
///
/// `None` (unset — the driver default) is always legal, exactly as the TypeScript
/// `value === null || value === undefined` short-circuit. Any present token must be
/// a member of [`reasoning_effort_options_for_runtime`]. This is the authoritative
/// check every write boundary shares.
pub fn reasoning_effort_valid_for_runtime(
    kind: RuntimeKind,
    value: Option<&str>,
    model: Option<&str>,
) -> bool {
    match value {
        None => true,
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
            RuntimeKind::Claude,
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
    fn codex_accepts_its_baseline_and_rejects_qoder_none_and_model_gated_max() {
        for token in ["low", "medium", "high", "xhigh"] {
            assert!(reasoning_effort_valid_for_runtime(
                RuntimeKind::Codex,
                Some(token),
                None
            ));
        }
        // `none` is qoder's; it must not leak onto codex.
        assert!(!reasoning_effort_valid_for_runtime(
            RuntimeKind::Codex,
            Some("none"),
            None
        ));
        // `max`/`ultra` are model-gated: illegal on the conservative baseline (driver
        // default, unknown, or custom model).
        for model in [None, Some("gpt-5.5"), Some("some-custom-id")] {
            assert!(!reasoning_effort_valid_for_runtime(
                RuntimeKind::Codex,
                Some("max"),
                model
            ));
            assert!(!reasoning_effort_valid_for_runtime(
                RuntimeKind::Codex,
                Some("ultra"),
                model
            ));
        }
    }

    #[test]
    fn codex_gpt6_family_accepts_ultra_without_widening_other_models() {
        for model in ["gpt-6", "gpt-6-astra", "gpt-6-future", " gpt-6-astra "] {
            for effort in ["max", "ultra"] {
                assert!(reasoning_effort_valid_for_runtime(
                    RuntimeKind::Codex,
                    Some(effort),
                    Some(model)
                ));
            }
        }
        for model in [
            None,
            Some(""),
            Some("gpt-6-"),
            Some("gpt-60"),
            Some("gpt-6astra"),
            Some("proxy/gpt-6-astra"),
            Some("gpt-5.6-future"),
            Some("gpt-5.6-terra"),
        ] {
            assert!(!reasoning_effort_valid_for_runtime(
                RuntimeKind::Codex,
                Some("ultra"),
                model
            ));
        }
        assert!(!reasoning_effort_valid_for_runtime(
            RuntimeKind::Grok,
            Some("ultra"),
            Some("gpt-6-astra")
        ));
    }

    #[test]
    fn codex_per_model_overrides_are_exact_and_not_prefix_matched() {
        // sol exposes both max and ultra.
        assert!(reasoning_effort_valid_for_runtime(
            RuntimeKind::Codex,
            Some("ultra"),
            Some("gpt-5.6-sol")
        ));
        for model in ["gpt-6.1-sol", "gpt-6.1-terra"] {
            for effort in ["max", "ultra"] {
                assert!(reasoning_effort_valid_for_runtime(
                    RuntimeKind::Codex,
                    Some(effort),
                    Some(model)
                ));
            }
        }
        assert!(reasoning_effort_valid_for_runtime(
            RuntimeKind::Codex,
            Some("max"),
            Some("gpt-5.6-sol")
        ));
        // terra/luna expose max but NOT ultra.
        for model in ["gpt-5.6-terra", "gpt-5.6-luna"] {
            assert!(reasoning_effort_valid_for_runtime(
                RuntimeKind::Codex,
                Some("max"),
                Some(model)
            ));
            assert!(
                !reasoning_effort_valid_for_runtime(RuntimeKind::Codex, Some("ultra"), Some(model)),
                "{model} must not expose ultra"
            );
        }
        // A future 5.6 id is NOT prefix-matched: it stays on the conservative baseline.
        assert!(!reasoning_effort_valid_for_runtime(
            RuntimeKind::Codex,
            Some("max"),
            Some("gpt-5.6-nova")
        ));
    }

    #[test]
    fn a_blank_model_falls_back_to_the_runtime_baseline() {
        // `model?.trim()` treats whitespace-only as absent → baseline, so a
        // model-gated token stays illegal.
        assert!(!reasoning_effort_valid_for_runtime(
            RuntimeKind::Codex,
            Some("max"),
            Some("   ")
        ));
        // And a baseline token stays legal.
        assert!(reasoning_effort_valid_for_runtime(
            RuntimeKind::Codex,
            Some("high"),
            Some("")
        ));
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
