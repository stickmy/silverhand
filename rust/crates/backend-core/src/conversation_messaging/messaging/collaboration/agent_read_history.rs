//! The agent read-history window domain: the pure anchor/window decision behind
//! `POST /internal/agents/{aid}/read-history`.
//!
//! Four window modes, mirroring `buildReadHistoryDto`: `before_seq` (the `limit`
//! messages with `seq < before`), `after_seq` (the `limit` with `seq > after`),
//! `around_message_id` (the anchor centred, `floor((limit-1)/2)` before + the
//! anchor + the rest after), and the head window (the latest `limit`) when no
//! anchor is given. At most one of the three explicit anchors may be present. The
//! adapter runs the SQL; this module owns the mode selection and the around split.

/// The requested window, already admitted (at most one anchor, limit in range).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadHistoryAnchor {
    /// The latest `limit` messages (no explicit anchor).
    Head,
    /// The `limit` messages with `seq < before_seq`.
    BeforeSeq(i64),
    /// The `limit` messages with `seq > after_seq`.
    AfterSeq(i64),
    /// The window centred on the message with `around_message_id`.
    Around(String),
}

/// The around split, mirroring the reference: `beforeCount = floor((limit-1)/2)`
/// messages strictly before the anchor seq, then the anchor row, then the remaining
/// budget after. Returned so the adapter issues exactly the reference's three reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AroundSplit {
    pub before_count: i64,
}

/// Compute the around split's before-budget for a `limit`. `floor((limit-1)/2)`.
pub fn around_before_count(limit: i64) -> AroundSplit {
    AroundSplit {
        before_count: (limit - 1).max(0) / 2,
    }
}

/// The message snippet, mirroring the reference `snippetFor`: ECMAScript-whitespace
/// collapsed + trimmed; if ≤160 UTF-16 code units, as-is; else the first 159 units +
/// '…'. Shares the ONE production text authority with the search excerpt:
/// the SAME ECMAScript `\s` set (U+FEFF collapses, U+0085 does not) and a
/// SCALAR-SAFE truncation that never splits an astral surrogate pair into U+FFFD.
pub fn snippet_for(content: &str) -> String {
    const MAX: usize = 160;
    let compact = crate::foundation::name_canonicalization::collapse_ecmascript_whitespace(content);
    crate::foundation::name_canonicalization::truncate_utf16_scalar_safe(&compact, MAX)
}

/// One projected history message. `author` is the resolved domain author (the
/// server projects it to `MessageSenderRef`; `None` = system). `metadata`,
/// `attachments`, and `reactions` are SEALED, already-parsed JSON values:
/// the persistence adapter decodes + validates them in the authorized
/// snapshot and fails closed on malformed JSON, so a malformed row can NEVER reach
/// the route as a silent `200`-`null`. `metadata` is a JSON object or `Null`;
/// `attachments`/`reactions` are JSON arrays (an empty array is `[]`, distinct from a
/// parse failure which is a typed store error, not `Null`). `content` is carried only
/// when `include_content`.
#[derive(Clone, Debug)]
pub struct ReadHistoryMessage {
    pub message_id: String,
    pub conversation_id: String,
    pub seq: i64,
    pub created_at: i64,
    pub thread_root_id: Option<String>,
    pub author: Option<crate::conversation_messaging::messaging::MessageAuthor>,
    pub snippet: String,
    pub metadata: serde_json::Value,
    pub attachments: serde_json::Value,
    pub reactions: serde_json::Value,
    pub content: String,
    pub options: Option<super::message_options::MessageOptionsState>,
}

/// The committed window read: the projected messages (ascending seq) plus whether
/// more exist before/after the window edges, and the target's full placement
/// coordinates (resolved in the SAME authorized snapshot, so the ref and the rows
/// can never disagree).
#[derive(Clone, Debug)]
pub struct ReadHistoryWindow {
    pub messages: Vec<ReadHistoryMessage>,
    pub has_more_before: bool,
    pub has_more_after: bool,
    pub target: super::message_target_coordinates::MessageTargetCoordinates,
}

/// Why a read-history read refused, decided INSIDE the authorized snapshot so a
/// revoke/rename/delete race cannot leak an old window. The target
/// existence + agent participation + workspace `read_messages` gates all live here,
/// not in a pre-snapshot route preflight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadHistoryRefusal {
    /// The `around_message_id` names no message in the (thread-scoped) conversation
    /// — reference `404 message_not_found`.
    AnchorNotFound,
    /// The target conversation does not exist — `404 conversation not found`.
    ConversationNotFound,
    /// The agent is not a live participant of the target — the reference refusal.
    NotParticipant,
    /// The agent lacks `read_messages` on the target's workspace — `403
    /// capability_denied`.
    CapabilityDenied,
}

/// Reads one conversation's message-history window for an agent as ONE authorized
/// connection-bound snapshot: the target existence +
/// placement coordinates, the agent's live participation, the workspace
/// `read_messages` gate (a `direct_pair` has no capability axis), the window rows,
/// the resolved authors, and the reaction aggregates (with `viewer_reacted` for THIS
/// agent) all come from the same transaction. The route resolves only the target
/// address + thread root; it does NOT pre-check placement/capability against a
/// separate pool read, and it projects the result without a second live read.
pub trait AgentReadHistoryRead: Send + Sync + 'static {
    #[allow(clippy::too_many_arguments)]
    fn read_history(
        &self,
        agent: &crate::foundation::identity::AgentId,
        conversation: &crate::foundation::identity::ConversationId,
        thread_root: Option<&str>,
        anchor: &ReadHistoryAnchor,
        include_system: bool,
        include_content: bool,
        limit: i64,
    ) -> impl core::future::Future<
        Output = Result<
            Result<ReadHistoryWindow, ReadHistoryRefusal>,
            crate::foundation::persistence::PersistenceFailure,
        >,
    > + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn around_splits_the_budget_below_the_anchor() {
        // limit 50 → 24 before, anchor, up to 25 after.
        assert_eq!(around_before_count(50).before_count, 24);
        assert_eq!(around_before_count(1).before_count, 0);
        assert_eq!(around_before_count(2).before_count, 0);
        assert_eq!(around_before_count(3).before_count, 1);
    }

    #[test]
    fn snippet_uses_the_ecmascript_whitespace_set_not_rust_is_whitespace() {
        // U+FEFF is JS `\s` (collapses); U+0085 NEL is NOT JS `\s` (kept
        // verbatim). Rust's char::is_whitespace() would invert both.
        assert_eq!(
            snippet_for("a\u{FEFF}b"),
            "a b",
            "U+FEFF collapses (JS \\s)"
        );
        assert_eq!(
            snippet_for("a\u{0085}b"),
            "a\u{0085}b",
            "U+0085 NEL is NOT JS whitespace — kept verbatim, not collapsed"
        );
        // Ordinary run collapse + trim.
        assert_eq!(snippet_for("  x   y \t z  "), "x y z");
    }

    #[test]
    fn snippet_truncation_is_scalar_safe_at_the_160_unit_astral_boundary() {
        // an astral char is 2 UTF-16 units. Build content whose 160th unit
        // is the LOW half of a surrogate pair, so a naive `units[..159]` slice would
        // split it into U+FFFD. The scalar-safe truncation must shrink inward.
        // 158 ASCII + one astral (2 units) => the pair straddles units 159..160.
        let mut content = "a".repeat(158);
        content.push('\u{1F600}'); // 😀 — units 159 (high) + 160 (low)
        content.push_str(&"b".repeat(20)); // push over the 160 cap
        let snip = snippet_for(&content);
        assert_eq!(snip, format!("{}…", "a".repeat(158)));
    }
}
