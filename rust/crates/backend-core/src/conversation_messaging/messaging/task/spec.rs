//! Task aggregate value objects: the ubiquitous language every task consumer
//! (write path, read models, wire projection) shares. Pure domain data — no
//! driver, no clock, no IO.
//!
//! Each textual or numeric field that later feeds an invariant is a validated
//! domain value, not a raw primitive. [`TaskNumber`] rejects both the
//! non-positive values the `task_number > 0` CHECK forbids and values past the
//! JSON-safe integer ceiling; [`TaskTitle`] carries only the
//! normalized, length-bounded canonical form; [`Assignee`], [`Creator`], and
//! their siblings make the "at most one of" / "all-or-nothing" column
//! invariants *unrepresentable* rather than merely checked. A persisted row
//! that violates these is corruption, rejected at the decode boundary (a
//! safe-500 concern), never smuggled into the aggregate as a legal value.
//!
//! Wire and column (de)serialization of the *envelope* — the JSON shape, the
//! request/response DTOs — stays an adapter concern. The status TOKEN does not:
//! `tasks.status`'s CHECK set and the wire enum are the same five strings, one
//! bijection, so it is owned here beside the enum ([`TaskStatus::as_str`] /
//! [`TaskStatus::parse`]) rather than re-matched in each adapter. Two
//! hand-written matches would be two authorities for one alphabet, and they
//! drift silently — a wire-side typo still passes every persistence test.
//! (`schedule::spec` leaves `ScheduleStatus` a bare enum with the column text in
//! its adapter; that is existing debt to close when the schedule wire lands, not
//! a shape to copy.)

use crate::foundation::identity::{AgentId, UserId};

/// Persisted task lifecycle status. Mirrors the `tasks.status` CHECK set and
/// the `schema.ts` enum. The legal transitions between these — and which are
/// reachable only through claim/dispatch rather than the `status` verb — are a
/// policy concern of the mutation layer, not this value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    Todo,
    InProgress,
    InReview,
    Done,
    Closed,
}

/// A persisted or received status token outside the closed set. At a read
/// boundary this is persistence corruption — the SQLite adapter maps it to its
/// persistence-failure seam (a safe 500), never to a product 400 — and on the
/// wire it is a request-classification failure the schema rejects first.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("unknown task status token")]
pub struct UnknownTaskStatus;

impl TaskStatus {
    /// Every arm, so consumers and gates can enumerate the closed set instead
    /// of restating it. Adding a variant is a compile error in [`Self::as_str`]
    /// below; the round-trip test then pins this array against that match.
    pub const ALL: [TaskStatus; 5] = [
        TaskStatus::Todo,
        TaskStatus::InProgress,
        TaskStatus::InReview,
        TaskStatus::Done,
        TaskStatus::Closed,
    ];

    /// The one canonical token for this status — the `tasks.status` column value
    /// and the wire value, which are the same string.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Todo => "todo",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::InReview => "in_review",
            TaskStatus::Done => "done",
            TaskStatus::Closed => "closed",
        }
    }

    /// The inverse of [`Self::as_str`]. Exact match only: no case folding, no
    /// trimming, no aliases — the column and the wire both carry exactly these
    /// five bytes, and accepting anything else would invent a second alphabet.
    pub fn parse(token: &str) -> Result<Self, UnknownTaskStatus> {
        match token {
            "todo" => Ok(TaskStatus::Todo),
            "in_progress" => Ok(TaskStatus::InProgress),
            "in_review" => Ok(TaskStatus::InReview),
            "done" => Ok(TaskStatus::Done),
            "closed" => Ok(TaskStatus::Closed),
            _ => Err(UnknownTaskStatus),
        }
    }
}

/// The task aggregate's own identity (`tasks.id`), an opaque coordinate. The
/// only invariant at a read boundary is non-emptiness: a row whose primary key
/// decodes to the empty string is persistence corruption, never a legal task.
/// This is a task-domain id, so it lives with the task bounded context rather
/// than the cross-capability `crate::foundation::identity` home.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TaskId(String);

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("task id must not be empty")]
pub struct EmptyTaskId;

impl TaskId {
    pub fn parse(raw: impl Into<String>) -> Result<Self, EmptyTaskId> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(EmptyTaskId);
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

/// A task's per-board sequence number (`tasks.task_number`), allocated
/// `MAX(task_number) + 1` within a board. Two invariants bound it, and this is
/// the ONE minter that enforces both — generation, persisted-row decode, and the
/// query grammar all mint through here, so there is a single `TaskNumber`
/// contract rather than a per-site range:
///   * strict positivity, the load-bearing invariant the `task_number > 0` CHECK
///     guarantees — a zero or negative persisted value is corruption;
///   * at most [`TaskNumber::MAX_SAFE`] (JavaScript's `Number.MAX_SAFE_INTEGER`).
///     The wire carries a `task_number` as a JSON number, so a value past the
///     safe integer would lose precision the moment a TS/JS consumer parses it —
///     the same number could round to a different task. Capping the domain keeps
///     one decimal denoting one task across TS, JSON, and Rust. Widening past it
///     would require moving the wire to a decimal string / BigInt first.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TaskNumber(i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("task_number must be a positive integer at most Number.MAX_SAFE_INTEGER")]
pub struct InvalidTaskNumber;

impl TaskNumber {
    /// The largest representable task number — JavaScript's `Number.MAX_SAFE_INTEGER`
    /// (`2^53 - 1`), the ceiling below which the JSON-number wire stays exact on every
    /// consumer. The query grammar's out-of-range message reads this same constant.
    pub const MAX_SAFE: i64 = 9_007_199_254_740_991;

    pub fn new(number: i64) -> Result<Self, InvalidTaskNumber> {
        if number <= 0 || number > Self::MAX_SAFE {
            return Err(InvalidTaskNumber);
        }
        Ok(Self(number))
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// Max length of a task title, counted in UTF-16 code units — the unit the
/// wire schema's `.max()` measures, NOT graphemes: an astral char counts as 2.
pub const TASK_TITLE_MAX_LEN: usize = 200;

/// Max length of a task body/content, in UTF-16 code units.
pub const TASK_CONTENT_MAX_LEN: usize = 10_000;

/// A task title in its canonical form: leading/trailing whitespace stripped and
/// every interior run of whitespace collapsed to a single U+0020, then required
/// non-empty and at most [`TASK_TITLE_MAX_LEN`] UTF-16 code units. The cap is
/// measured on the *canonical* form (raw runs of whitespace never inflate it),
/// matching the wire `TaskTitleSchema` (`.trim().replace(/\s+/g, ' ')` then
/// `.min(1).max(200)`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskTitle(String);

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum InvalidTitle {
    #[error("title is empty after whitespace normalization")]
    Empty,
    #[error("title exceeds {TASK_TITLE_MAX_LEN} UTF-16 code units after normalization")]
    TooLong,
}

impl TaskTitle {
    pub fn parse(raw: &str) -> Result<Self, InvalidTitle> {
        let normalized = collapse_ecmascript_whitespace(raw);
        if normalized.is_empty() {
            return Err(InvalidTitle::Empty);
        }
        if utf16_len(&normalized) > TASK_TITLE_MAX_LEN {
            return Err(InvalidTitle::TooLong);
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

/// A task body. Stored verbatim — no trim, no whitespace collapse; the empty
/// string is legal (the `content` column defaults to `''`). The UTF-16 bound
/// (wire `TaskBodySchema`) applies to body input via `parse` only; reads use
/// `from_persisted` and accept any length.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskContent(String);

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("content exceeds {TASK_CONTENT_MAX_LEN} UTF-16 code units")]
pub struct ContentTooLong;

impl TaskContent {
    pub fn parse(raw: impl Into<String>) -> Result<Self, ContentTooLong> {
        let raw = raw.into();
        if utf16_len(&raw) > TASK_CONTENT_MAX_LEN {
            return Err(ContentTooLong);
        }
        Ok(Self(raw))
    }

    /// A stored body. Never length-checked: the bound is an input contract.
    pub fn from_persisted(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The default body: an empty string (the column's `DEFAULT ''`).
    pub fn empty() -> Self {
        Self(String::new())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

/// Who created a task. The three arms encode the `creator_kind` CHECK triple
/// exactly — an agent creator carries an agent id and no user id, a user
/// creator the reverse, and a system creator neither — so the illegal
/// combinations the CHECK forbids cannot be constructed at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Creator {
    Agent(AgentId),
    User(UserId),
    System,
}

/// A task's assignee. "Unassigned" is *both* assignee columns null; the CHECK
/// permits at most one non-null, which this type makes unrepresentable.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Assignee {
    #[default]
    Unassigned,
    Agent(AgentId),
    User(UserId),
}

/// The ECMAScript `\s` set — WhiteSpace ∪ LineTerminator — which is the exact
/// set the wire `TaskTitleSchema`'s `.trim()` and `/\s+/g` act on.
///
/// It is *listed* rather than delegated to `char::is_whitespace` because the
/// two disagree on two characters reachable in a title, and a normalized title
/// is compared against the other side's output character for character:
/// U+FEFF (a byte-order mark) is NOT Unicode `White_Space` but IS ECMAScript
/// whitespace, so it must collapse; U+0085 (NEL) IS Unicode `White_Space` but
/// is NOT ECMAScript whitespace, so it must be preserved. The authority is the
/// ECMAScript spec; this listing is pinned to it by the tests below.
/// (`messaging::mention_cc` lists the same set for its own excerpt contract; a
/// shared `core` text util could unify the two, but that crosses a
/// bounded-context boundary and is left as separate consolidation.)
fn is_ecmascript_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'
            | '\u{a}'
            | '\u{b}'
            | '\u{c}'
            | '\u{d}'
            | '\u{20}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `raw.trim().replace(/\s+/g, ' ')`: collapse every run of ECMAScript
/// whitespace to one U+0020 and drop the leading/trailing runs.
fn collapse_ecmascript_whitespace(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_space = false;
    for c in raw.chars() {
        if is_ecmascript_whitespace(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
            continue;
        }
        in_space = false;
        out.push(c);
    }
    // Trim the ECMAScript-whitespace ends — NOT `str::trim`, whose Unicode
    // `White_Space` set would also strip a leading/trailing U+0085 NEL that the
    // ECMAScript contract keeps. After the collapse above, any leading/trailing
    // ECMAScript-whitespace run is already a single U+0020, so this removes it.
    out.trim_matches(is_ecmascript_whitespace).to_owned()
}

/// Length in UTF-16 code units — the unit the wire length caps are measured in.
fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_is_opaque_but_never_empty() {
        assert_eq!(TaskId::parse(""), Err(EmptyTaskId));
        let id = TaskId::parse("task_abc").unwrap();
        assert_eq!(id.as_str(), "task_abc");
        assert_eq!(id.into_inner(), "task_abc");
    }

    #[test]
    fn task_number_is_bounded_to_the_json_safe_range() {
        // Below the floor: zero and negatives are corruption / invalid.
        assert_eq!(TaskNumber::new(0), Err(InvalidTaskNumber));
        assert_eq!(TaskNumber::new(-1), Err(InvalidTaskNumber));
        assert_eq!(TaskNumber::new(1).unwrap().get(), 1);
        // At the ceiling: Number.MAX_SAFE_INTEGER round-trips exactly on the wire.
        assert_eq!(TaskNumber::MAX_SAFE, 9_007_199_254_740_991);
        assert_eq!(
            TaskNumber::new(TaskNumber::MAX_SAFE).unwrap().get(),
            TaskNumber::MAX_SAFE
        );
        // One past it a JSON consumer would round, so it is out of the domain — the
        // single minter refuses it for generation, decode, and query alike.
        assert_eq!(
            TaskNumber::new(TaskNumber::MAX_SAFE + 1),
            Err(InvalidTaskNumber)
        );
        assert_eq!(TaskNumber::new(i64::MAX), Err(InvalidTaskNumber));
    }

    #[test]
    fn content_is_verbatim_and_only_length_bounded() {
        // No trim / no collapse: leading whitespace and interior runs survive.
        let content = TaskContent::parse("  keep   me  ").unwrap();
        assert_eq!(content.as_str(), "  keep   me  ");

        // Empty is legal (the column default).
        assert_eq!(TaskContent::empty().as_str(), "");
        assert_eq!(TaskContent::parse("").unwrap().as_str(), "");
    }

    #[test]
    fn content_length_cap_is_utf16_code_units() {
        assert!(TaskContent::parse("a".repeat(10_000)).is_ok());
        assert_eq!(TaskContent::parse("a".repeat(10_001)), Err(ContentTooLong));
        // 5000 astral chars = 10_000 UTF-16 units (ok); 5001 = 10_002 (too long).
        assert!(TaskContent::parse("\u{1f600}".repeat(5_000)).is_ok());
        assert_eq!(
            TaskContent::parse("\u{1f600}".repeat(5_001)),
            Err(ContentTooLong)
        );
    }

    #[test]
    fn assignee_default_is_unassigned() {
        assert_eq!(Assignee::default(), Assignee::Unassigned);
    }

    #[test]
    fn status_token_round_trips_every_arm() {
        for status in TaskStatus::ALL {
            assert_eq!(TaskStatus::parse(status.as_str()), Ok(status), "{status:?}");
        }
        // `ALL` really is the closed set: five arms, five distinct tokens, and
        // exactly the alphabet the `tasks.status` CHECK and `TaskStatusSchema`
        // both spell out.
        let tokens: std::collections::BTreeSet<&str> =
            TaskStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(tokens.len(), TaskStatus::ALL.len());
        assert_eq!(
            tokens.into_iter().collect::<Vec<_>>(),
            vec!["closed", "done", "in_progress", "in_review", "todo"]
        );
    }

    #[test]
    fn status_parse_is_exact_and_admits_nothing_else() {
        // No case folding, no trimming, no aliases, no legacy spellings: each of
        // these would be a second alphabet, and a persisted one is corruption.
        for bad in [
            "",
            "Todo",
            "TODO",
            " todo",
            "todo ",
            "in-progress",
            "inprogress",
            "in_Review",
            "cancelled",
            "in_progress\n",
        ] {
            assert_eq!(TaskStatus::parse(bad), Err(UnknownTaskStatus), "{bad:?}");
        }
    }
}

/// The TS-signed half of [`TaskTitle`]: the production `TaskTitleSchema` is the
/// authority for the canonical form. The expected values were captured from
/// that schema before the TypeScript backend was retired and remain an
/// independent frozen oracle for the production Rust parser.
#[cfg(test)]
mod title_parity {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/task_title_parity.json");

    fn fixture() -> serde_json::Value {
        serde_json::from_str(FIXTURE).expect("signed fixture is valid JSON")
    }

    #[test]
    fn classifier_agrees_with_the_signed_set_over_the_whole_code_space() {
        let signed: std::collections::HashSet<u32> = fixture()["ecmascript_whitespace_code_points"]
            .as_array()
            .expect("signed code point array")
            .iter()
            .map(|v| u32::try_from(v.as_u64().expect("code point is a number")).expect("scalar"))
            .collect();
        // The count is pinned deliberately: if ECMAScript ever grows or loses a
        // whitespace character, the regenerated fixture must be seen on this
        // side too, not absorbed silently.
        assert_eq!(signed.len(), 25, "signed ECMAScript whitespace set size");
        assert!(signed.contains(&0xfeff), "U+FEFF is ECMAScript whitespace");
        assert!(!signed.contains(&0x85), "U+0085 NEL is not");

        // Sweep every scalar value, not a sample: a member this classifier
        // misses AND a non-member it wrongly accepts both fail here. This is
        // what makes `char::is_whitespace` unusable rather than merely unwise.
        for cp in 0..=0x10_FFFF_u32 {
            let Some(c) = char::from_u32(cp) else {
                continue; // surrogate code points are not scalar values
            };
            assert_eq!(
                is_ecmascript_whitespace(c),
                signed.contains(&cp),
                "U+{cp:04X} classification"
            );
        }
    }

    #[test]
    fn production_parse_reproduces_every_signed_title_case() {
        let fixture = fixture();
        let cases = fixture["titles"].as_array().expect("signed title cases");
        // Guards a truncated or hand-trimmed fixture from passing as coverage:
        // 17 hand-written cases plus one per whitespace member.
        assert!(cases.len() >= 42, "signed case count: {}", cases.len());

        for case in cases {
            let name = case["name"].as_str().expect("case name");
            let raw = case["raw"].as_str().expect("case input");
            match (TaskTitle::parse(raw), case["canonical"].as_str()) {
                (Ok(title), Some(canonical)) => assert_eq!(title.as_str(), canonical, "{name}"),
                (Err(refusal), None) => {
                    let expected = match case["refusal"].as_str().expect("refusal arm") {
                        "empty" => InvalidTitle::Empty,
                        "too_long" => InvalidTitle::TooLong,
                        other => panic!("{name}: unknown refusal arm {other:?}"),
                    };
                    assert_eq!(refusal, expected, "{name}");
                }
                (got, expected) => {
                    panic!("{name}: accept/reject disagreement — rust {got:?}, ts {expected:?}")
                }
            }
        }
    }
}
