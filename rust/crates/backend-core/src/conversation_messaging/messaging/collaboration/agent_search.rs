//! The agent-scoped message search read model: a LIKE search over the messages an
//! agent participates in, scoped to the calling conversation, a named conversation,
//! or a workspace. Mirrors `buildAgentSearchMessagesDto`.
//!
//! The participation guard is the union of the workspace roster edge
//! (`conversation_agents`) and the direct-pair agent endpoint — a message an agent
//! cannot reach is never a hit. The excerpt + `match_offset` reproduce the
//! reference `searchExcerptFor` byte-for-byte on the wire: whitespace-collapsed,
//! case-insensitive first match, ±80 context chars with `...` affixes, and a
//! `match_offset` in JavaScript UTF-16 code units.

use std::future::Future;

use crate::foundation::name_canonicalization::{fold_case_only, fold_case_only_with_spans};
use crate::foundation::persistence::PersistenceFailure;

/// How the search was scoped, derived (never asserted twice) from the selector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentSearchScope {
    /// No selector: the calling conversation (`current_workspace`-less default).
    Current,
    /// A `conversation_id` selector.
    Conversation,
    /// A `workspace_id` / `current_workspace` selector.
    Workspace,
}

impl AgentSearchScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Conversation => "conversation",
            Self::Workspace => "workspace",
        }
    }
}

/// The excerpt context window each side of the match, in JS UTF-16 code units.
const SEARCH_EXCERPT_CONTEXT_CHARS: usize = 80;

/// One search excerpt: the surrounding text with `...` affixes and the match offset
/// within it, in UTF-16 code units (the reference `searchExcerptFor`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchExcerpt {
    pub excerpt: String,
    pub match_offset: i64,
}

/// Compute the excerpt + `match_offset` for `content` against `query`, matching the
/// (synced) reference `searchExcerptFor` under the PORTABLE full case fold:
/// collapse whitespace + trim; fold BOTH sides with the pinned
/// fold-only primitive (no NFC, isomorphic to TS `foldCase`); find the first match in
/// FOLDED space; map the folded match-start back to the ORIGINAL text's UTF-16 offset
/// via the per-code-point fold-span map (a full fold can expand, e.g. `ß`→`ss`, so a
/// naive same-offset slice would drift — #22); take ±80 UTF-16 units of the ORIGINAL
/// compact text with `...` affixes, SCALAR-SAFE (a window edge that lands inside a
/// surrogate pair shrinks inward by one unit, never `from_utf16_lossy` — #27). A
/// non-match yields the leading `2*80` window at offset 0.
pub fn search_excerpt_for(content: &str, query: &str) -> SearchExcerpt {
    let compact = collapse_whitespace(content);
    let units: Vec<u16> = compact.encode_utf16().collect();

    // Fold both sides (fold-only, no NFC) and carry the content's per-char span map.
    let (folded_content, spans) = fold_case_only_with_spans(&compact);
    let folded_query = fold_case_only(query);
    let folded_haystack: Vec<u16> = folded_content.encode_utf16().collect();
    let folded_needle: Vec<u16> = folded_query.encode_utf16().collect();

    let Some(folded_match_start) = find_subslice(&folded_haystack, &folded_needle) else {
        let end = scalar_safe_end((SEARCH_EXCERPT_CONTEXT_CHARS * 2).min(units.len()), &units);
        return SearchExcerpt {
            excerpt: string_from_utf16(&units[..end]),
            match_offset: 0,
        };
    };

    // Map the folded UTF-16 match RANGE `[start, start+needle_len)` back to the
    // MINIMAL complete ORIGINAL scalar interval that covers it: a
    // full fold can EXPAND (`ß`→`ss`), so a match that begins or ends part-way through
    // an expansion must map to the WHOLE original scalar it overlaps — never past its
    // start (which would land on the next scalar) nor short of its end. E.g.
    // `content='ßa', query='sa'` (fold `ssa`, match at folded 1) covers original
    // `[0,2)` = `ßa`, NOT `[1,..)` = `a`.
    let folded_needle_len = folded_needle.len();
    let (match_start, match_end) =
        folded_range_to_original(folded_match_start, folded_needle_len, &spans);

    let raw_start = match_start.saturating_sub(SEARCH_EXCERPT_CONTEXT_CHARS);
    let raw_end = (match_end + SEARCH_EXCERPT_CONTEXT_CHARS).min(units.len());
    let start = scalar_safe_start(raw_start, &units);
    let end = scalar_safe_end(raw_end, &units);
    let prefix = if start > 0 { "..." } else { "" };
    let suffix = if end < units.len() { "..." } else { "" };
    let middle = string_from_utf16(&units[start..end]);
    SearchExcerpt {
        excerpt: format!("{prefix}{middle}{suffix}"),
        match_offset: (match_start - start + prefix.encode_utf16().count()) as i64,
    }
}

/// Map a FOLDED UTF-16 range `[folded_start, folded_start+folded_len)` to the MINIMAL
/// complete ORIGINAL scalar interval `[orig_start, orig_end)` that covers it. A fold
/// span maps one original scalar's UTF-16 width to its folded width (which may be
/// larger, e.g. `ß`→`ss`). The match may begin/end MID-span; the covering interval
/// starts at the BEGINNING of the span containing `folded_start` and ends at the END
/// of the span containing the last folded unit (`folded_start+folded_len-1`), so a
/// partial-expansion match never drifts to a neighbouring scalar. An empty
/// folded range (defensive) maps to a zero-width interval at the containing offset.
fn folded_range_to_original(
    folded_start: usize,
    folded_len: usize,
    spans: &[crate::foundation::name_canonicalization::FoldSpan],
) -> (usize, usize) {
    let folded_end_inclusive = folded_start + folded_len.max(1) - 1;
    let mut folded_cursor = 0usize;
    let mut original_cursor = 0usize;
    let mut orig_start = 0usize;
    let mut orig_end = 0usize;
    let mut saw_start = false;
    for span in spans {
        let folded_width = span.folded_utf16 as usize;
        let original_width = span.original_utf16 as usize;
        let span_folded_begin = folded_cursor;
        let span_folded_end = folded_cursor + folded_width; // exclusive
        // The span that CONTAINS folded_start begins the original interval.
        if !saw_start && folded_start < span_folded_end {
            orig_start = original_cursor;
            saw_start = true;
        }
        // The span that CONTAINS the last folded unit ends the original interval.
        if folded_end_inclusive < span_folded_end && folded_end_inclusive >= span_folded_begin {
            orig_end = original_cursor + original_width;
            break;
        }
        folded_cursor = span_folded_end;
        original_cursor += original_width;
        // If the range extends to the very end, `orig_end` trails the cursor.
        orig_end = original_cursor;
    }
    if !saw_start {
        // folded_start is at/after the end (empty content or trailing) — clamp.
        orig_start = original_cursor;
        orig_end = original_cursor;
    }
    if orig_end < orig_start {
        orig_end = orig_start;
    }
    (orig_start, orig_end)
}

/// Shrink a window START inward if it lands on a low surrogate (the tail half of a
/// pair): move to `+1` so the slice begins on a whole scalar.
fn scalar_safe_start(offset: usize, units: &[u16]) -> usize {
    if offset < units.len() && is_low_surrogate(units[offset]) {
        offset + 1
    } else {
        offset
    }
}

/// Shrink a window END inward if it would split a pair — i.e. the unit just before
/// `end` is a high surrogate (the lead half): drop it so the slice ends on a whole
/// scalar. Never exceeds the original cap.
fn scalar_safe_end(offset: usize, units: &[u16]) -> usize {
    if offset > 0 && offset <= units.len() && is_high_surrogate(units[offset - 1]) {
        offset - 1
    } else {
        offset
    }
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}
fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

/// Collapse every run of ECMAScript `\s` whitespace to one space and trim, matching
/// `content.replace(/\s+/g, ' ').trim()`. Delegates to the ONE production text
/// authority the read-history snippet also uses, so both share the exact
/// ECMAScript `\s` set (U+FEFF in, U+0085 out) — never Rust's `char::is_whitespace()`.
fn collapse_whitespace(content: &str) -> String {
    crate::foundation::name_canonicalization::collapse_ecmascript_whitespace(content)
}

fn find_subslice(haystack: &[u16], needle: &[u16]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

fn string_from_utf16(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// The domain query the read authority consumes: the trimmed search text, the
/// derived scope, the resolved coordinates, the system filter, and the limit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSearchQuery {
    pub text: String,
    pub include_system: bool,
    pub limit: i64,
}

/// The requested search scope selector, already admitted at the wire (at most one of
/// the id selectors, `current_workspace` true-or-absent). The read authority resolves
/// it against the caller's source in ONE snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchScopeSelector {
    /// No selector — the calling conversation (an `AgentSlot` with no source refuses).
    Current,
    /// `current_workspace: true` — the source conversation's workspace.
    CurrentWorkspace,
    /// An explicit `workspace_id`.
    Workspace(crate::foundation::identity::WorkspaceId),
    /// An explicit `conversation_id`.
    Conversation(crate::foundation::identity::ConversationId),
}

/// Why an agent search refused, decided IN the authorized snapshot.
/// The `conversation_id` arm's provenance matrix and the workspace cap gate all live
/// here, not in a pre-snapshot route preflight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchRefusal {
    /// An `AgentSlot` (no source) used the default/`current_workspace` scope without
    /// naming one — `400 search_scope_required`.
    ScopeRequired,
    /// A `current_workspace` scope on a workspace-less source — `400
    /// workspace_scope_required`.
    WorkspaceScopeRequired,
    /// The caller's bound source conversation no longer exists — `404 not found`.
    SourceNotFound,
    /// A cross-workspace target the agent cannot reach — `403 forbidden`.
    WorkspaceForbidden,
    /// The named target conversation does not exist — `404 conversation_not_found`.
    ConversationNotFound,
    /// The agent is not a participant of the named target — `403
    /// not_conversation_member`.
    NotConversationMember,
    /// The agent lacks `read_messages` on the target workspace — `403
    /// capability_denied`.
    CapabilityDenied,
}

/// The resolved scope echoed back on success (the wire `scope` + coordinate echoes).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSearchScope {
    pub scope: AgentSearchScope,
    pub echo_workspace: Option<String>,
    pub echo_conversation: Option<String>,
}

/// The committed search result: the resolved scope echo + the ordered hits.
#[derive(Clone, Debug)]
pub struct AgentSearchResult {
    pub scope: ResolvedSearchScope,
    pub hits: Vec<AgentSearchHit>,
}

/// Reads the agent's message search hits as ONE authorized connection-bound snapshot:
/// resolve the scope against the caller's source + gate
/// cross-workspace reach + `read_messages` + participation, then continuously
/// fold-evaluate candidates (ordered `created_at DESC, seq DESC`) until `limit` is
/// filled or the scope is exhausted, resolving authors in the SAME transaction. The
/// route resolves nothing but the raw selector; it projects the result.
pub trait AgentSearchRead: Send + Sync + 'static {
    fn search(
        &self,
        agent: &crate::foundation::identity::AgentId,
        source: Option<&crate::foundation::identity::ConversationId>,
        selector: &SearchScopeSelector,
        query: &AgentSearchQuery,
    ) -> impl Future<Output = Result<Result<AgentSearchResult, SearchRefusal>, PersistenceFailure>> + Send;
}

/// One projected search hit: the message reference WITH its per-hit placement
/// coordinates (a workspace search spans conversations, so each hit carries its own),
/// the resolved author (`None` = system), and the excerpt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSearchHit {
    pub message_id: String,
    pub conversation_id: String,
    pub seq: i64,
    pub created_at: i64,
    pub author: Option<crate::conversation_messaging::messaging::MessageAuthor>,
    pub excerpt: SearchExcerpt,
    pub coordinates: super::message_target_coordinates::MessageTargetCoordinates,
    pub options: Option<super::message_options::MessageOptionsState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_centres_on_the_first_case_insensitive_match_with_affixes() {
        let content = "The quick BROWN fox jumps over the lazy dog";
        let hit = search_excerpt_for(content, "brown");
        // Short content (< 2*80): no affixes, whole string, offset at 'BROWN'.
        assert_eq!(hit.excerpt, content);
        assert_eq!(hit.match_offset, "The quick ".encode_utf16().count() as i64);
    }

    #[test]
    fn whitespace_is_collapsed_before_matching() {
        let hit = search_excerpt_for("a\n\n  b   \tc", "b c");
        assert_eq!(hit.excerpt, "a b c");
        assert_eq!(hit.match_offset, 2);
    }

    #[test]
    fn excerpt_shares_the_ecmascript_whitespace_set_with_the_snippet() {
        // The SAME set as the read-history snippet — U+FEFF collapses, U+0085
        // NEL does not (Rust's char::is_whitespace() would invert both).
        let feff = search_excerpt_for("a\u{FEFF}b", "zzz");
        assert_eq!(feff.excerpt, "a b", "U+FEFF collapses (JS \\s)");
        let nel = search_excerpt_for("a\u{0085}b", "zzz");
        assert_eq!(
            nel.excerpt, "a\u{0085}b",
            "U+0085 NEL kept verbatim (not JS whitespace)"
        );
    }

    #[test]
    fn long_content_gets_affixes_and_a_windowed_offset() {
        let prefix = "x".repeat(200);
        let suffix = "y".repeat(200);
        let content = format!("{prefix}NEEDLE{suffix}");
        let hit = search_excerpt_for(&content, "needle");
        assert_eq!(
            hit.excerpt,
            format!("...{}NEEDLE{}...", "x".repeat(80), "y".repeat(80))
        );
        // 80 context chars + the 3-char prefix affix.
        assert_eq!(hit.match_offset, (80 + 3) as i64);
    }

    #[test]
    fn portable_fold_matches_expanding_and_greek_case_and_keeps_original_offset() {
        // `ß` folds to `ss` (EXPANDS): a query `SS` must match the `ß` and the offset
        // must map back to the ORIGINAL text's `ß` position, not the folded one.
        let hit = search_excerpt_for("straße", "SS");
        assert_eq!(hit.excerpt, "straße");
        assert_eq!(hit.match_offset, "stra".encode_utf16().count() as i64);

        // Greek capital sigma vs lowercase/final sigma — full fold makes them equal.
        let hit = search_excerpt_for("ΣΟΦΟΣ here", "σοφος");
        assert_eq!(hit.match_offset, 0);
        // Final-sigma variant folds the same.
        let hit = search_excerpt_for("word σοφός end", "ΣΟΦΌΣ");
        assert_eq!(hit.match_offset, "word ".encode_utf16().count() as i64);
    }

    #[test]
    fn search_excerpt_replays_the_ts_signed_parity_fixture() {
        // The Rust search_excerpt_for reproduces the TS searchExcerptFor
        // excerpt + match_offset cell-for-cell (the SAME shared corpus the TS signer
        // regenerates), so the two fold-span implementations cannot diverge on the
        // expanding / mid-expansion / astral cases.
        const FIXTURE: &str = include_str!("../../../../tests/fixtures/search-excerpt.parity.json");
        let value: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let cells = value["cells"].as_array().expect("cells array");
        for cell in cells {
            let label = cell["label"].as_str().unwrap();
            let content = cell["content"].as_str().unwrap();
            let query = cell["query"].as_str().unwrap();
            let expected_excerpt = cell["excerpt"].as_str().unwrap();
            let expected_offset = cell["match_offset"].as_i64().unwrap();
            let hit = search_excerpt_for(content, query);
            assert_eq!(hit.excerpt, expected_excerpt, "cell {label} excerpt");
            assert_eq!(hit.match_offset, expected_offset, "cell {label} offset");
        }
    }

    #[test]
    fn scalar_safe_window_never_splits_a_surrogate_pair() {
        // The ASCII padding makes both ±80 window edges fall inside emoji pairs.
        // Each edge must shrink inward by one UTF-16 unit.
        let filler = "😀".repeat(90);
        let content = format!("{filler}xNEEDLEx{filler}");
        let hit = search_excerpt_for(&content, "needle");
        assert_eq!(
            hit.excerpt,
            format!("...{}xNEEDLEx{}...", "😀".repeat(39), "😀".repeat(39))
        );
        assert_eq!(hit.match_offset, 82);

        // The unmatched leading window has its own 160-unit end boundary.
        let hit = search_excerpt_for(&format!("x{filler}"), "absent");
        assert_eq!(hit.excerpt, format!("x{}", "😀".repeat(79)));
        assert_eq!(hit.match_offset, 0);
    }

    #[test]
    fn combining_sequence_folds_without_nfc_composition() {
        // fold-only (no NFC): a decomposed `e` + combining acute stays two code
        // points; matching a decomposed query aligns; the offset maps to the original.
        let content = "x cafe\u{0301} time";
        let hit = search_excerpt_for(content, "CAFE\u{0301}");
        assert_eq!(hit.excerpt, content);
        assert_eq!(hit.match_offset, 2);

        // A composed query must not match the decomposed text through NFC.
        let hit = search_excerpt_for(content, "CAFÉ");
        assert_eq!(hit.excerpt, content);
        assert_eq!(hit.match_offset, 0);
    }
}
