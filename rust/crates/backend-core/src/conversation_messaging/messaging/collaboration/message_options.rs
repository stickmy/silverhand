//! Answer options on a message and its one current answer (#task:3143).
//!
//! The author declares a closed set of options; any other participant answers by
//! picking options and/or giving free "other" text. A re-answer replaces the answer.
//! Options may change only while unanswered, and only by their author.

use std::{collections::HashSet, error::Error, future::Future};

use crate::foundation::identity::{AgentId, ConversationId, UserId};

use super::super::{MessageId, PrincipalIdentity};

pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 10;
pub const MAX_OPTION_ID_CHARS: usize = 16;
pub const MAX_LABEL_CHARS: usize = 200;
pub const MAX_OTHER_CHARS: usize = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionMode {
    Single,
    Multi,
}

impl OptionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Multi => "multi",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "single" => Some(Self::Single),
            "multi" => Some(Self::Multi),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OptionItem {
    id: String,
    label: String,
}

impl OptionItem {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("message options are outside the published grammar")]
pub struct InvalidOptions;

/// A validated option set: 2-10 items, ids `[a-z0-9][a-z0-9_-]{0,15}` and unique,
/// labels trimmed, non-empty and at most 200 chars.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageOptions {
    mode: OptionMode,
    items: Vec<OptionItem>,
}

impl MessageOptions {
    pub fn parse(
        mode: OptionMode,
        raw: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, InvalidOptions> {
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        for (id, label) in raw {
            let label = trim(&label).to_owned();
            if !valid_option_id(&id)
                || !seen.insert(id.clone())
                || label.is_empty()
                || label.chars().count() > MAX_LABEL_CHARS
            {
                return Err(InvalidOptions);
            }
            items.push(OptionItem { id, label });
        }
        if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&items.len()) {
            return Err(InvalidOptions);
        }
        Ok(Self { mode, items })
    }

    pub fn mode(&self) -> OptionMode {
        self.mode
    }

    pub fn items(&self) -> &[OptionItem] {
        &self.items
    }

    pub fn item(&self, id: &str) -> Option<&OptionItem> {
        self.items.iter().find(|item| item.id == id)
    }
}

fn valid_option_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    id.len() <= MAX_OPTION_ID_CHARS
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The ECMAScript `.trim()` set the zod schema applies; Rust's `char::is_whitespace`
/// differs on U+FEFF and U+0085.
fn trim(value: &str) -> &str {
    value.trim_matches(|c: char| {
        matches!(
            c,
            '\u{9}'..='\u{D}'
                | '\u{20}'
                | '\u{A0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
        )
    })
}

/// Who answered, with the names snapshotted at answer time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Answerer {
    principal: PrincipalIdentity,
    name: String,
    display_name: String,
}

impl Answerer {
    pub fn new(principal: PrincipalIdentity, name: String, display_name: String) -> Self {
        Self {
            principal,
            name,
            display_name,
        }
    }

    pub fn principal(&self) -> &PrincipalIdentity {
        &self.principal
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageAnswer {
    by: Answerer,
    option_ids: Vec<String>,
    other: Option<String>,
    answered_at_ms: i64,
}

impl MessageAnswer {
    pub fn stored(
        by: Answerer,
        option_ids: Vec<String>,
        other: Option<String>,
        answered_at_ms: i64,
    ) -> Self {
        Self {
            by,
            option_ids,
            other,
            answered_at_ms,
        }
    }

    pub fn by(&self) -> &Answerer {
        &self.by
    }

    pub fn option_ids(&self) -> &[String] {
        &self.option_ids
    }

    pub fn other(&self) -> Option<&str> {
        self.other.as_deref()
    }

    pub fn answered_at_ms(&self) -> i64 {
        self.answered_at_ms
    }
}

/// A message's options and its current answer, as every reader sees them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageOptionsState {
    options: MessageOptions,
    answer: Option<MessageAnswer>,
}

impl MessageOptionsState {
    pub fn new(options: MessageOptions, answer: Option<MessageAnswer>) -> Self {
        Self { options, answer }
    }

    pub fn options(&self) -> &MessageOptions {
        &self.options
    }

    pub fn answer(&self) -> Option<&MessageAnswer> {
        self.answer.as_ref()
    }
}

/// The picks an answer request carries, before they are checked against the stored
/// options. Shape only: `other` trimmed, non-empty, at most 500 chars.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnswerPicks {
    option_ids: Vec<String>,
    other: Option<String>,
}

impl AnswerPicks {
    pub fn parse(option_ids: Vec<String>, other: Option<String>) -> Result<Self, AnswerRefusal> {
        let other = match other {
            None => None,
            Some(raw) => {
                let text = trim(&raw);
                if text.is_empty() || text.chars().count() > MAX_OTHER_CHARS {
                    return Err(AnswerRefusal::InvalidAnswer);
                }
                Some(text.to_owned())
            }
        };
        if option_ids.is_empty() && other.is_none() || option_ids.len() > MAX_OPTIONS {
            return Err(AnswerRefusal::InvalidAnswer);
        }
        Ok(Self { option_ids, other })
    }

    /// Check against the current options; returns the picked ids in option order.
    pub fn resolve(&self, options: &MessageOptions) -> Result<Vec<String>, AnswerRefusal> {
        let mut unique = HashSet::new();
        for id in &self.option_ids {
            if options.item(id).is_none() {
                return Err(AnswerRefusal::UnknownOption);
            }
            if !unique.insert(id.as_str()) {
                return Err(AnswerRefusal::InvalidAnswer);
            }
        }
        if options.mode() == OptionMode::Single
            && self.option_ids.len() + usize::from(self.other.is_some()) != 1
        {
            return Err(AnswerRefusal::InvalidAnswer);
        }
        Ok(options
            .items()
            .iter()
            .filter(|item| unique.contains(item.id()))
            .map(|item| item.id().to_owned())
            .collect())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OptionsActor {
    Human(UserId),
    Agent {
        id: AgentId,
        source: Option<ConversationId>,
    },
}

impl OptionsActor {
    pub fn principal(&self) -> PrincipalIdentity {
        match self {
            Self::Human(id) => PrincipalIdentity::Human(id.clone()),
            Self::Agent { id, .. } => PrincipalIdentity::Agent(id.clone()),
        }
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, Self::Agent { .. })
    }
}

/// Picks and options arrive unvalidated so a hidden or forbidden target refuses
/// before a malformed body does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnswerCommand {
    pub actor: OptionsActor,
    pub message: MessageId,
    pub option_ids: Vec<String>,
    pub other: Option<String>,
    pub occurred_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateOptionsCommand {
    pub actor: OptionsActor,
    pub message: MessageId,
    pub options: Result<MessageOptions, InvalidOptions>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AnswerRefusal {
    #[error("message is outside the caller's visible scope")]
    TargetHidden,
    #[error("message not found")]
    MessageNotFound,
    #[error("caller cannot post in this conversation")]
    Forbidden,
    #[error("runtime lacks the send_messages capability")]
    CapabilityDenied,
    #[error("conversation is archived")]
    ConversationArchived,
    #[error("message carries no options")]
    NoOptions,
    #[error("an option id is not in the message's options")]
    UnknownOption,
    #[error("answer does not fit the option mode")]
    InvalidAnswer,
    #[error("the author cannot answer their own question")]
    OwnQuestion,
    #[error("options cannot change once answered")]
    OptionsAnswered,
    #[error("only the author may update options")]
    NotAuthor,
    #[error("message options are outside the published grammar")]
    InvalidOptions,
}

/// Decide an answer against the stored state: who may answer and what they may pick.
/// Returns the picked items in option order.
pub fn decide_answer<'a>(
    state: &'a MessageOptionsState,
    question_author: Option<&PrincipalIdentity>,
    answerer: &PrincipalIdentity,
    option_ids: Vec<String>,
    other: Option<String>,
) -> Result<(Vec<&'a OptionItem>, Option<String>), AnswerRefusal> {
    if question_author == Some(answerer) {
        return Err(AnswerRefusal::OwnQuestion);
    }
    let picks = AnswerPicks::parse(option_ids, other)?;
    let ids = picks.resolve(state.options())?;
    let items = ids
        .iter()
        .filter_map(|id| state.options().item(id))
        .collect();
    Ok((items, picks.other))
}

/// Decide an options update: only the author, only while unanswered.
pub fn decide_update(
    state: &MessageOptionsState,
    question_author: Option<&PrincipalIdentity>,
    actor: &PrincipalIdentity,
) -> Result<(), AnswerRefusal> {
    if question_author != Some(actor) {
        return Err(AnswerRefusal::NotAuthor);
    }
    if state.answer().is_some() {
        return Err(AnswerRefusal::OptionsAnswered);
    }
    Ok(())
}

/// The system event an answer posted, committed in the same transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnswerEventMessage {
    pub id: MessageId,
    pub content: String,
    pub metadata: String,
    pub seq: i64,
    pub created_at_ms: i64,
}

/// Where the question lives and who asked it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionCoordinates {
    pub message: MessageId,
    pub conversation: ConversationId,
    pub thread_root: Option<MessageId>,
    pub author: PrincipalIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnsweredFact {
    pub question: QuestionCoordinates,
    pub state: MessageOptionsState,
    pub event: AnswerEventMessage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OptionsUpdatedFact {
    pub question: QuestionCoordinates,
    pub state: MessageOptionsState,
}

pub trait MessageOptionsPort {
    type Error: Error + Send + Sync + 'static;

    fn answer(
        &self,
        command: AnswerCommand,
    ) -> impl Future<Output = Result<Result<AnsweredFact, AnswerRefusal>, Self::Error>> + Send;

    fn update_options(
        &self,
        command: UpdateOptionsCommand,
    ) -> impl Future<Output = Result<Result<OptionsUpdatedFact, AnswerRefusal>, Self::Error>> + Send;
}

/// Post-commit effects: publish the event message and the options change, wake the
/// question's author. Failures are reported, never turned into a refusal.
pub trait MessageOptionsAftermath {
    fn answered(&self, fact: &AnsweredFact) -> impl Future<Output = Vec<String>> + Send;
    fn options_updated(
        &self,
        fact: &OptionsUpdatedFact,
    ) -> impl Future<Output = Vec<String>> + Send;
}

/// A named principal as the answer event spells it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedPrincipal {
    pub principal: PrincipalIdentity,
    pub name: String,
}

fn principal_json(named: &NamedPrincipal) -> serde_json::Value {
    let kind = match named.principal {
        PrincipalIdentity::Agent(_) => "agent",
        PrincipalIdentity::Human(_) => "user",
    };
    serde_json::json!({ "kind": kind, "id": named.principal.as_str(), "name": named.name })
}

/// The `message_answer` system event: content line and metadata JSON.
pub fn answer_event(
    question: &MessageId,
    question_author: &NamedPrincipal,
    answerer: &NamedPrincipal,
    picked: &[&OptionItem],
    other: Option<&str>,
    changed: bool,
) -> (String, String) {
    let mut parts: Vec<String> = picked.iter().map(|item| item.label().to_owned()).collect();
    if let Some(other) = other {
        parts.push(format!("Other: {other}"));
    }
    let content = if changed {
        format!(
            "@{} changed the answer: {}",
            answerer.name,
            parts.join(", ")
        )
    } else {
        format!(
            "@{} answered @{}'s question: {}",
            answerer.name,
            question_author.name,
            parts.join(", ")
        )
    };
    let metadata = serde_json::json!({
        "subtype": "message_answer",
        "answer_to": question.as_str(),
        "question_author": principal_json(question_author),
        "answerer": principal_json(answerer),
        "options": picked
            .iter()
            .map(|item| serde_json::json!({ "id": item.id(), "label": item.label() }))
            .collect::<Vec<_>>(),
        "other": other,
        "changed": changed,
    });
    (content, metadata.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(mode: OptionMode) -> MessageOptions {
        MessageOptions::parse(
            mode,
            [
                ("a".into(), " A ".into()),
                ("b".into(), "B".into()),
                ("c".into(), "C".into()),
            ],
        )
        .unwrap()
    }

    #[test]
    fn options_grammar() {
        assert_eq!(options(OptionMode::Single).items()[0].label(), "A");
        for bad in [
            vec![("a", "A")],
            vec![("a", "A"), ("a", "B")],
            vec![("A", "A"), ("b", "B")],
            vec![("-a", "A"), ("b", "B")],
            vec![("a", "  "), ("b", "B")],
            vec![("abcdefghijklmnopq", "A"), ("b", "B")],
        ] {
            let raw = bad.into_iter().map(|(i, l)| (i.to_owned(), l.to_owned()));
            assert_eq!(
                MessageOptions::parse(OptionMode::Single, raw),
                Err(InvalidOptions)
            );
        }
        let eleven = (0..11).map(|i| (format!("o{i}"), "x".to_owned()));
        assert!(MessageOptions::parse(OptionMode::Multi, eleven).is_err());
    }

    #[test]
    fn single_takes_exactly_one_pick() {
        let single = options(OptionMode::Single);
        let pick = |ids: &[&str], other: Option<&str>| {
            AnswerPicks::parse(
                ids.iter().map(|s| s.to_string()).collect(),
                other.map(str::to_owned),
            )
            .and_then(|p| p.resolve(&single))
        };
        assert_eq!(pick(&["b"], None), Ok(vec!["b".to_owned()]));
        assert_eq!(pick(&[], Some("x")), Ok(vec![]));
        assert_eq!(pick(&["a", "b"], None), Err(AnswerRefusal::InvalidAnswer));
        assert_eq!(pick(&["a"], Some("x")), Err(AnswerRefusal::InvalidAnswer));
        assert_eq!(pick(&["z"], None), Err(AnswerRefusal::UnknownOption));
        assert_eq!(pick(&[], Some("  ")), Err(AnswerRefusal::InvalidAnswer));
        assert_eq!(pick(&[], None), Err(AnswerRefusal::InvalidAnswer));
    }

    #[test]
    fn multi_returns_option_order_and_combines_other() {
        let multi = options(OptionMode::Multi);
        let picks = AnswerPicks::parse(vec!["c".into(), "a".into()], Some("x".into())).unwrap();
        assert_eq!(
            picks.resolve(&multi),
            Ok(vec!["a".to_owned(), "c".to_owned()])
        );
        let dup = AnswerPicks::parse(vec!["a".into(), "a".into()], None).unwrap();
        assert_eq!(dup.resolve(&multi), Err(AnswerRefusal::InvalidAnswer));
    }
}
