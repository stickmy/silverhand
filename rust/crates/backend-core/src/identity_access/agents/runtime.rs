//! Which runtimes an agent may report, as one closed set.

/// The exact runtimes an agent may report.
///
/// The set is closed and lives in the agent domain because "which runtimes exist"
/// is a fact about agents, not about the message that names one or the wire that
/// renders it. A stored value outside the set is a broken database, so
/// [`RuntimeKind::parse`] answers `None` and leaves it to the adapter to fail closed
/// with the offending value attached — this type never invents a default.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeKind {
    Claude,
    Codex,
    Cursor,
    Gemini,
    Qoder,
    Opencode,
    Pi,
    Grok,
    Antigravity,
}

impl RuntimeKind {
    /// The stored spelling this runtime is written and read as. The single source
    /// of the accepted set, so a reader and a writer cannot drift apart.
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeKind::Claude => "claude",
            RuntimeKind::Codex => "codex",
            RuntimeKind::Cursor => "cursor",
            RuntimeKind::Gemini => "gemini",
            RuntimeKind::Qoder => "qoder",
            RuntimeKind::Opencode => "opencode",
            RuntimeKind::Pi => "pi",
            RuntimeKind::Grok => "grok",
            RuntimeKind::Antigravity => "antigravity",
        }
    }

    /// `None` for anything outside the closed set. Deliberately not a fallback to a
    /// default runtime: an unknown stored value is a data-invariant fault the
    /// adapter must surface, not a caller's problem to paper over.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "claude" => Some(RuntimeKind::Claude),
            "codex" => Some(RuntimeKind::Codex),
            "cursor" => Some(RuntimeKind::Cursor),
            "gemini" => Some(RuntimeKind::Gemini),
            "qoder" => Some(RuntimeKind::Qoder),
            "opencode" => Some(RuntimeKind::Opencode),
            "pi" => Some(RuntimeKind::Pi),
            "grok" => Some(RuntimeKind::Grok),
            "antigravity" => Some(RuntimeKind::Antigravity),
            _ => None,
        }
    }

    /// Every runtime in the set, so a caller that must cover all of them reads the
    /// owner instead of restating the tokens.
    pub const ALL: [RuntimeKind; 9] = [
        RuntimeKind::Claude,
        RuntimeKind::Codex,
        RuntimeKind::Cursor,
        RuntimeKind::Gemini,
        RuntimeKind::Qoder,
        RuntimeKind::Opencode,
        RuntimeKind::Pi,
        RuntimeKind::Grok,
        RuntimeKind::Antigravity,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_set_round_trips_and_rejects_anything_outside_it() {
        for runtime in RuntimeKind::ALL {
            assert_eq!(RuntimeKind::parse(runtime.as_str()), Some(runtime));
        }
        // Fails closed rather than defaulting: an unknown spelling is not a runtime.
        assert_eq!(RuntimeKind::parse("claude "), None);
        assert_eq!(RuntimeKind::parse(""), None);
        assert_eq!(RuntimeKind::parse("aider"), None);
    }

    #[test]
    fn all_names_every_variant_exactly_once() {
        // `ALL` is what other crates iterate to cover the set. The match below is
        // exhaustive over the enum, so a variant added without being listed here
        // fails to COMPILE rather than silently narrowing every caller that trusts
        // `ALL` to be the whole set.
        let mut seen = std::collections::BTreeSet::new();
        for kind in RuntimeKind::ALL {
            let spelling = match kind {
                RuntimeKind::Claude => "claude",
                RuntimeKind::Codex => "codex",
                RuntimeKind::Cursor => "cursor",
                RuntimeKind::Gemini => "gemini",
                RuntimeKind::Qoder => "qoder",
                RuntimeKind::Opencode => "opencode",
                RuntimeKind::Pi => "pi",
                RuntimeKind::Grok => "grok",
                RuntimeKind::Antigravity => "antigravity",
            };
            assert_eq!(kind.as_str(), spelling);
            assert!(seen.insert(spelling), "{spelling} is listed twice");
        }
        assert_eq!(seen.len(), RuntimeKind::ALL.len());
    }
}
