//! The list query grammar: the raw `?workspace_id=…&status=…&task_number=…` string an
//! agent's `task list` sends → the validated [`ListTasksQuery`] coordinate. The parity of
//! the TS route's `ListTasksQuerySchema` parse plus its pre-validation massaging.
//!
//! The single-value keys take the FIRST occurrence and treat an empty value as absent (the
//! parity of Hono's `c.req.query(k) || undefined`); the repeatable keys (`status`,
//! `task_number`) collect EVERY occurrence. `status` drops the legacy `all` alias before
//! validation, and an empty result adds no filter. Percent-decoding is the caller's
//! (`form_urlencoded`), so every value here is already decoded.
//!
//! REFINEMENTS OVER THE TS COERCION, surfaced deliberately (not a silent divergence). "Strict
//! decimal" here means both LEXICAL strictness and CROSS-LANGUAGE exactness:
//!   * lexical — a numeric field parses as a strict base-10 integer rather than JavaScript
//!     `Number()` coercion, so the canonical `?task_number=190` / `?limit=50` forms are
//!     identical, but the quirk forms `Number()` would accept (`190.0`, `0x10`, ` 190 `,
//!     `1e2`) are refused;
//!   * exactness — a `task_number` must be at most [`TaskNumber::MAX_SAFE`] (JavaScript's
//!     `Number.MAX_SAFE_INTEGER`). Above it a JSON number can no longer represent every
//!     integer uniquely, so the TS schema would silently ROUND a larger value to a different
//!     integer than the digits the agent typed — two sides both "accept" while pointing at
//!     different tasks. This ceiling is NOT a query-local rule: it is the core [`TaskNumber`]
//!     value-object invariant, minted in ONE place shared by generation, persisted-row decode,
//!     and this grammar — the query reuses the minter rather than keeping a second cap. A
//!     token past it is [`QueryFieldReason::OutOfRange`], never a silently different selector.
//!     An out-of-range decimal is still a decimal, so it is a range failure, not a grammar
//!     failure (`limit`'s far smaller `.max(500)` bound makes the safe-integer ceiling moot for
//!     it, but the same range classification applies).
//!
//! `limit` keeps the TS asymmetry exactly: an empty `?limit=` is absent (no page bound), while
//! an empty `?task_number=` is an invalid value, because the reference guards only the former
//! with `|| undefined`.

use std::num::IntErrorKind;

use silverhand_backend_core::conversation_messaging::messaging::task::{TaskNumber, TaskStatus};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, WorkspaceId};

/// The largest page a caller may ask for — the parity of the TS `LimitSchema`'s `.max(500)`.
const MAX_LIMIT: i64 = 500;

/// The validated list coordinate the handler resolves a board scope for and narrows the
/// fetch by. Every field is a narrowing; an absent/empty one adds no clause.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ListTasksQuery {
    /// The explicit `--workspace-id` selector that pins the single-board lane.
    pub workspace_id: Option<WorkspaceId>,
    /// The explicit `--workspace <name>` selector; resolved by the route.
    pub workspace_name: Option<String>,
    /// A `--target` address naming the one conversation to list; resolved by the route.
    pub target: Option<String>,
    /// Narrow to one conversation.
    pub conversation_id: Option<ConversationId>,
    /// Narrow to a set of statuses; empty = every status.
    pub statuses: Vec<TaskStatus>,
    /// Narrow to a set of board-local numbers; empty = every number.
    pub task_numbers: Vec<TaskNumber>,
    /// Narrow to one assignee agent.
    pub assignee_agent: Option<AgentId>,
    /// The page bound applied to the VISIBLE rows; `None` = every visible row.
    pub limit: Option<usize>,
}

/// The query named a value outside its schema. Carries WHICH field failed and WHY, so the
/// `400` can hand the agent a specific sentence naming the field and the rule it broke, while
/// the machine `code` stays the stable `invalid_query` a caller can branch on. The offending
/// raw value is not echoed and the reference's zod `issues` array is not reproduced — a
/// field + reason sentence is the contract (see the Acceptance Summary).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InvalidQuery {
    pub field: QueryField,
    pub reason: QueryFieldReason,
}

/// The query field whose value was rejected — the name echoed back to the agent verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueryField {
    Status,
    TaskNumber,
    Limit,
    Selector,
}

/// Why a field's value was rejected — the three schema rules a list query can break.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueryFieldReason {
    /// Not one of a fixed set of accepted tokens (an unknown `status`).
    NotInClosedSet,
    /// Not a strict base-10 integer — a fraction, hex, surrounding space, or empty. An
    /// all-digit value too large for `i64` is NOT this: it is a well-formed decimal that is
    /// merely out of range, reported as [`Self::OutOfRange`].
    NotABase10Integer,
    /// A well-formed decimal integer outside the permitted range — non-positive, past the
    /// field's cap, or too large to represent (a `task_number` above [`TaskNumber::MAX_SAFE`]
    /// or either field's value beyond `i64`).
    OutOfRange,
    /// `target` with another scope key, or `workspace_id` with `workspace_name`.
    Conflicting,
}

impl QueryField {
    /// The wire name of the query key, as the agent spelled it.
    fn as_str(self) -> &'static str {
        match self {
            QueryField::Status => "status",
            QueryField::TaskNumber => "task_number",
            QueryField::Limit => "limit",
            QueryField::Selector => "target",
        }
    }
}

impl InvalidQuery {
    fn of(field: QueryField, reason: QueryFieldReason) -> Self {
        Self { field, reason }
    }

    /// The specific agent-facing sentence: which field, and which rule it broke.
    pub(crate) fn message(&self) -> String {
        let field = self.field.as_str();
        match self.reason {
            QueryFieldReason::Conflicting => {
                "target excludes workspace_id, workspace_name and conversation_id; \
                 workspace_id excludes workspace_name"
                    .to_owned()
            }
            QueryFieldReason::NotInClosedSet => {
                // List the closed set so the agent can correct the value, not just learn it
                // was wrong. Built from the status enum's own array, so it never drifts.
                let accepted = TaskStatus::ALL.map(|status| status.as_str()).join(", ");
                format!("query field '{field}' must be one of: {accepted}")
            }
            QueryFieldReason::NotABase10Integer => {
                format!("query field '{field}' must be a base-10 integer")
            }
            QueryFieldReason::OutOfRange => {
                let high = match self.field {
                    QueryField::Limit => MAX_LIMIT,
                    // The task_number ceiling is the core TaskNumber invariant, read from its
                    // one constant so the message can never drift from the minter that enforces it.
                    _ => TaskNumber::MAX_SAFE,
                };
                format!("query field '{field}' must be an integer between 1 and {high}")
            }
        }
    }
}

/// Parse the raw (already percent-decoded by `form_urlencoded`) query pairs into the
/// validated coordinate, or [`InvalidQuery`] for a value outside its schema.
pub(crate) fn parse_list_query(raw: Option<&str>) -> Result<ListTasksQuery, InvalidQuery> {
    let mut workspace_id: Option<String> = None;
    let mut workspace_name: Option<String> = None;
    let mut target: Option<String> = None;
    let mut conversation_id: Option<String> = None;
    let mut assignee_id: Option<String> = None;
    let mut limit: Option<String> = None;
    let mut statuses: Vec<String> = Vec::new();
    let mut task_numbers: Vec<String> = Vec::new();

    if let Some(raw) = raw {
        for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
            match key.as_ref() {
                "workspace_id" => set_first(&mut workspace_id, value.into_owned()),
                "workspace_name" => set_first(&mut workspace_name, value.into_owned()),
                "target" => set_first(&mut target, value.into_owned()),
                "conversation_id" => set_first(&mut conversation_id, value.into_owned()),
                "assignee_id" => set_first(&mut assignee_id, value.into_owned()),
                "limit" => set_first(&mut limit, value.into_owned()),
                "status" => statuses.push(value.into_owned()),
                "task_number" => task_numbers.push(value.into_owned()),
                // An unlisted key is ignored, as the reference schema ignores members it
                // does not name rather than refusing them.
                _ => {}
            }
        }
    }

    let workspace_name = workspace_name.filter(|value| !value.is_empty());
    let target = target.filter(|value| !value.is_empty());
    let workspace_id = workspace_id.filter(|value| !value.is_empty());
    let conversation_id = conversation_id.filter(|value| !value.is_empty());
    let target_conflicts = target.is_some()
        && (workspace_id.is_some() || workspace_name.is_some() || conversation_id.is_some());
    if target_conflicts || (workspace_id.is_some() && workspace_name.is_some()) {
        return Err(InvalidQuery::of(
            QueryField::Selector,
            QueryFieldReason::Conflicting,
        ));
    }

    Ok(ListTasksQuery {
        workspace_id: optional_id(workspace_id, WorkspaceId::parse),
        workspace_name,
        target,
        conversation_id: optional_id(conversation_id, ConversationId::parse),
        assignee_agent: optional_id(assignee_id, AgentId::parse),
        statuses: parse_statuses(&statuses)?,
        task_numbers: parse_task_numbers(&task_numbers)?,
        limit: parse_limit(limit.as_deref())?,
    })
}

/// Keep the FIRST occurrence, mirroring Hono's single-value `c.req.query`.
fn set_first(slot: &mut Option<String>, value: String) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

/// An empty value is absent (`|| undefined`); a non-empty one always parses, since these
/// ids reject only the empty string the filter already removed.
fn optional_id<T, E>(raw: Option<String>, parse: impl Fn(String) -> Result<T, E>) -> Option<T> {
    raw.filter(|value| !value.is_empty())
        .and_then(|value| parse(value).ok())
}

/// Drop the legacy `all` alias, then require every survivor to be a known status token.
fn parse_statuses(raw: &[String]) -> Result<Vec<TaskStatus>, InvalidQuery> {
    raw.iter()
        .filter(|token| token.as_str() != "all")
        .map(|token| {
            TaskStatus::parse(token)
                .map_err(|_| InvalidQuery::of(QueryField::Status, QueryFieldReason::NotInClosedSet))
        })
        .collect()
}

/// Parse a decimal token to `i64`, distinguishing a pure overflow (all-digit but beyond
/// `i64`) from a genuine grammar break. An out-of-range decimal is still a decimal, so the
/// agent-facing reason must say "out of range", never "not a base-10 integer".
fn parse_decimal(token: &str, field: QueryField) -> Result<i64, InvalidQuery> {
    token.parse::<i64>().map_err(|error| {
        let reason = match error.kind() {
            IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => QueryFieldReason::OutOfRange,
            _ => QueryFieldReason::NotABase10Integer,
        };
        InvalidQuery::of(field, reason)
    })
}

/// Every number must be a strictly-positive base-10 integer no larger than the JS-safe
/// ceiling, so the same decimal string denotes the same task on both sides. The ceiling is
/// NOT re-checked here: `parse_decimal` classifies an `i64`-overflowing token as `OutOfRange`,
/// and the core [`TaskNumber`] minter rejects both non-positive and past-`MAX_SAFE` values —
/// the one place the domain bound lives — which this maps onto the same `OutOfRange`.
fn parse_task_numbers(raw: &[String]) -> Result<Vec<TaskNumber>, InvalidQuery> {
    raw.iter()
        .map(|token| {
            let number = parse_decimal(token, QueryField::TaskNumber)?;
            TaskNumber::new(number)
                .map_err(|_| InvalidQuery::of(QueryField::TaskNumber, QueryFieldReason::OutOfRange))
        })
        .collect()
}

/// A limit is a positive integer at most [`MAX_LIMIT`]; an absent or empty one is no bound.
fn parse_limit(raw: Option<&str>) -> Result<Option<usize>, InvalidQuery> {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let limit = parse_decimal(raw, QueryField::Limit)?;
    if limit <= 0 || limit > MAX_LIMIT {
        return Err(InvalidQuery::of(
            QueryField::Limit,
            QueryFieldReason::OutOfRange,
        ));
    }
    Ok(Some(limit as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_query_narrows_nothing() {
        assert_eq!(parse_list_query(None).unwrap(), ListTasksQuery::default());
        assert_eq!(
            parse_list_query(Some("")).unwrap(),
            ListTasksQuery::default()
        );
    }

    #[test]
    fn empty_single_values_are_absent_not_errors() {
        // The parity of `c.req.query(k) || undefined`: an empty selector reads as unset.
        let query =
            parse_list_query(Some("workspace_id=&conversation_id=&assignee_id=&limit=")).unwrap();
        assert_eq!(query, ListTasksQuery::default());
    }

    #[test]
    fn target_and_workspace_name_are_read() {
        let query = parse_list_query(Some("target=%23conv%3Aabc")).unwrap();
        assert_eq!(query.target.as_deref(), Some("#conv:abc"));
        let query = parse_list_query(Some("workspace_name=alpha")).unwrap();
        assert_eq!(query.workspace_name.as_deref(), Some("alpha"));
    }

    #[test]
    fn selectors_are_mutually_exclusive() {
        for raw in [
            "target=x&workspace_id=w",
            "target=x&conversation_id=c",
            "target=x&workspace_name=n",
            "workspace_id=w&workspace_name=n",
        ] {
            assert_eq!(
                parse_list_query(Some(raw)),
                Err(InvalidQuery::of(
                    QueryField::Selector,
                    QueryFieldReason::Conflicting
                )),
                "{raw}"
            );
        }
    }

    #[test]
    fn single_values_take_the_first_occurrence() {
        let query = parse_list_query(Some("workspace_id=ws-1&workspace_id=ws-2")).unwrap();
        assert_eq!(
            query.workspace_id,
            Some(WorkspaceId::parse("ws-1").unwrap())
        );
    }

    #[test]
    fn status_is_repeatable_and_drops_the_all_alias() {
        let query =
            parse_list_query(Some("status=all&status=in_progress&status=in_review")).unwrap();
        assert_eq!(
            query.statuses,
            vec![TaskStatus::InProgress, TaskStatus::InReview]
        );
    }

    #[test]
    fn an_all_only_status_is_no_filter() {
        let query = parse_list_query(Some("status=all")).unwrap();
        assert!(query.statuses.is_empty());
    }

    #[test]
    fn an_unknown_status_is_invalid() {
        assert_eq!(
            parse_list_query(Some("status=frozen")),
            Err(InvalidQuery::of(
                QueryField::Status,
                QueryFieldReason::NotInClosedSet
            ))
        );
    }

    #[test]
    fn task_number_is_repeatable_and_positive_integers_only() {
        let query = parse_list_query(Some("task_number=190&task_number=191")).unwrap();
        assert_eq!(
            query.task_numbers,
            vec![TaskNumber::new(190).unwrap(), TaskNumber::new(191).unwrap()]
        );
    }

    #[test]
    fn non_positive_or_non_integer_task_numbers_are_invalid() {
        // Which reason the field failed for is part of the contract: a non-positive but
        // well-formed integer is a range break, a malformed one is a grammar break.
        for (raw, reason) in [
            ("task_number=0", QueryFieldReason::OutOfRange),
            ("task_number=-3", QueryFieldReason::OutOfRange),
            ("task_number=1.5", QueryFieldReason::NotABase10Integer),
            ("task_number=x", QueryFieldReason::NotABase10Integer),
            ("task_number=", QueryFieldReason::NotABase10Integer),
        ] {
            assert_eq!(
                parse_list_query(Some(raw)),
                Err(InvalidQuery::of(QueryField::TaskNumber, reason)),
                "{raw}"
            );
        }
    }

    #[test]
    fn limit_out_of_range_or_non_integer_is_invalid() {
        for (raw, reason) in [
            ("limit=0", QueryFieldReason::OutOfRange),
            ("limit=-1", QueryFieldReason::OutOfRange),
            ("limit=501", QueryFieldReason::OutOfRange),
            ("limit=1.5", QueryFieldReason::NotABase10Integer),
            ("limit=x", QueryFieldReason::NotABase10Integer),
        ] {
            assert_eq!(
                parse_list_query(Some(raw)),
                Err(InvalidQuery::of(QueryField::Limit, reason)),
                "{raw}"
            );
        }
    }

    #[test]
    fn task_numbers_above_the_js_safe_integer_are_out_of_range() {
        // The safe ceiling itself is accepted and denotes exactly that task; one past it,
        // i64::MAX, and a value that overflows i64 entirely are all range failures — never a
        // grammar failure, because the digits ARE a decimal, just too large to mean one task.
        assert_eq!(
            parse_list_query(Some("task_number=9007199254740991"))
                .unwrap()
                .task_numbers,
            vec![TaskNumber::new(9_007_199_254_740_991).unwrap()]
        );
        for raw in [
            "task_number=9007199254740992",    // MAX_SAFE + 1
            "task_number=9007199254740993",    // TS would round this to ...992
            "task_number=9223372036854775807", // i64::MAX
            "task_number=9223372036854775808", // i64::MAX + 1 (overflows the parse)
        ] {
            assert_eq!(
                parse_list_query(Some(raw)),
                Err(InvalidQuery::of(
                    QueryField::TaskNumber,
                    QueryFieldReason::OutOfRange
                )),
                "{raw}"
            );
        }
    }

    #[test]
    fn an_overflowing_limit_is_out_of_range_not_a_grammar_break() {
        // Beyond i64 the digits are still a decimal, so the reason is range, not grammar.
        assert_eq!(
            parse_list_query(Some("limit=99999999999999999999")),
            Err(InvalidQuery::of(
                QueryField::Limit,
                QueryFieldReason::OutOfRange
            ))
        );
    }

    #[test]
    fn each_invalid_reason_names_its_field_and_rule() {
        // The agent-facing sentence names the field and the specific rule it broke — the
        // three reasons shiki ruled the message must distinguish (closed-set / grammar /
        // range), never a flat "invalid query". The closed-set message lists the accepted
        // tokens, and a range message names the field's actual bound.
        assert_eq!(
            InvalidQuery::of(QueryField::Status, QueryFieldReason::NotInClosedSet).message(),
            "query field 'status' must be one of: todo, in_progress, in_review, done, closed"
        );
        assert_eq!(
            InvalidQuery::of(QueryField::TaskNumber, QueryFieldReason::NotABase10Integer).message(),
            "query field 'task_number' must be a base-10 integer"
        );
        assert_eq!(
            InvalidQuery::of(QueryField::TaskNumber, QueryFieldReason::OutOfRange).message(),
            "query field 'task_number' must be an integer between 1 and 9007199254740991"
        );
        assert_eq!(
            InvalidQuery::of(QueryField::Limit, QueryFieldReason::OutOfRange).message(),
            "query field 'limit' must be an integer between 1 and 500"
        );
    }

    #[test]
    fn percent_encoded_values_are_decoded() {
        let query = parse_list_query(Some("conversation_id=conv%2D1")).unwrap();
        assert_eq!(
            query.conversation_id,
            Some(ConversationId::parse("conv-1").unwrap())
        );
    }
}

/// The TS-signed half of the numeric/status grammar: the production
/// `ListTasksQuerySchema` is the authority, and shiki approved deliberate
/// refinements over it. A numeric field parses as a strict base-10 integer here,
/// where the schema's `z.coerce.number()` runs JavaScript `Number()`: the lexical
/// quirks `Number()` coerces (`190.0`, `0x10`, ` 190 `, `1e2`) are refused, and so
/// is any `task_number` past `Number.MAX_SAFE_INTEGER`, where a JSON number would
/// round to a DIFFERENT task than the digits name — TS "accepts" it (rounding when
/// it must), this grammar refuses it so one decimal string means one task on both
/// sides. Every other corpus input agrees. The historical TS schema oracle signed
/// this frozen corpus; its runner is no longer present. These tests preserve the
/// signed decisions and named refinements at the production raw-query boundary.
#[cfg(test)]
mod grammar_parity {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/list_query_parity.json");

    fn fixture() -> serde_json::Value {
        serde_json::from_str(FIXTURE).expect("signed fixture is valid JSON")
    }

    #[test]
    fn task_number_grammar_agrees_with_the_schema_except_the_named_refinement() {
        let fixture = fixture();
        let cases = fixture["task_number"]
            .as_array()
            .expect("signed task_number cases");
        // Guards a truncated fixture: the canonical, the shared rejections, the four lexical
        // refinements, and the safe-integer boundary family must all be present.
        assert!(cases.len() >= 16, "signed case count: {}", cases.len());

        let mut refinements = Vec::new();
        for case in cases {
            let name = case["name"].as_str().expect("case name");
            let raw = case["raw"].as_str().expect("case raw");
            let ts_accepts = case["ts_accepts"].as_bool().expect("ts_accepts");
            let refinement = case["rust_refinement"].as_bool().expect("rust_refinement");
            let encoded: String = form_urlencoded::byte_serialize(raw.as_bytes()).collect();
            let query = format!("task_number={encoded}");
            let parsed = parse_list_query(Some(&query)).map(|query| query.task_numbers);

            if refinement {
                refinements.push(raw.to_string());
                // A refinement is a form TS accepts but this grammar refuses; a
                // TS-rejected refinement would be a mislabelled shared rejection.
                assert!(ts_accepts, "{name}: a refinement must be TS-accepted");
                assert!(parsed.is_err(), "{name}: strict grammar refuses {raw:?}");
            } else if ts_accepts {
                let value = case["ts_value"].as_i64().expect("signed value");
                let expected = TaskNumber::new(value).expect("signed value is valid");
                assert_eq!(parsed.expect(name), vec![expected], "{name}");
            } else {
                assert!(parsed.is_err(), "{name}: shared rejection of {raw:?}");
            }
        }
        // The named refinements: four lexical `Number()` quirks plus the value-domain family
        // TS accepts (rounding when it must) and this grammar refuses to keep one string one
        // task. Pinning the exact set stops either kind silently disappearing.
        assert_eq!(
            refinements,
            vec![
                "190.0",
                "0x10",
                " 190 ",
                "1e2",
                "9007199254740992",
                "9007199254740993",
                "9223372036854775807",
                "9223372036854775808",
            ],
            "the named refinement forms"
        );
    }

    #[test]
    fn limit_grammar_agrees_with_the_schema_except_the_named_refinement() {
        let fixture = fixture();
        let cases = fixture["limit"].as_array().expect("signed limit cases");
        assert!(cases.len() >= 12, "signed case count: {}", cases.len());

        let mut refinements = 0;
        for case in cases {
            let name = case["name"].as_str().expect("case name");
            let raw = case["raw"].as_str().expect("case raw");
            let ts_accepts = case["ts_accepts"].as_bool().expect("ts_accepts");
            let refinement = case["rust_refinement"].as_bool().expect("rust_refinement");
            let encoded: String = form_urlencoded::byte_serialize(raw.as_bytes()).collect();
            let query = format!("limit={encoded}");
            let parsed = parse_list_query(Some(&query)).map(|query| query.limit);

            if refinement {
                refinements += 1;
                assert!(ts_accepts, "{name}: a refinement must be TS-accepted");
                assert!(parsed.is_err(), "{name}: strict grammar refuses {raw:?}");
            } else if ts_accepts {
                let value = case["ts_value"].as_u64().expect("signed value") as usize;
                assert_eq!(parsed.expect(name), Some(value), "{name}");
            } else {
                assert!(parsed.is_err(), "{name}: shared rejection of {raw:?}");
            }
        }
        assert_eq!(refinements, 4, "the four named refinement forms");
    }

    #[test]
    fn status_closed_set_agrees_with_the_schema() {
        let fixture = fixture();
        let cases = fixture["status"].as_array().expect("signed status cases");
        assert!(cases.len() >= 8, "signed case count: {}", cases.len());

        for case in cases {
            let name = case["name"].as_str().expect("case name");
            let raw = case["raw"].as_str().expect("case raw");
            let ts_accepts = case["ts_accepts"].as_bool().expect("ts_accepts");
            // The status axis is a pure closed set — no refinement, so accept /
            // reject must match the schema exactly.
            let encoded: String = form_urlencoded::byte_serialize(raw.as_bytes()).collect();
            let query = format!("status={encoded}");
            assert_eq!(parse_list_query(Some(&query)).is_ok(), ts_accepts, "{name}");
        }
    }
}
