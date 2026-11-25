//! Admission: what a send request must be, decided before any principal is known.
//!
//! ONE use-case, two orderable stages. This is the first, and it
//! owns every rule about the request's own content: shape, the UTF-8 content cap, the
//! attachment list's accepted forms and bounds, and the payload refine. It cannot see
//! a principal, an authority or a unit of work, and that is enforced by its signature
//! rather than by discipline: there is nothing in scope for it to consult.
//!
//! The second stage is [`super::execute`], which is where a principal and the write
//! unit finally appear. Splitting them this way is what lets the two doors run the
//! same rules in DIFFERENT ORDERS — the human door authorizes before it has a body,
//! the agent door validates before it resolves a target — without either door
//! carrying a second copy of a rule.
//!
//! THE TWO DOORS DO NOT ACCEPT THE SAME BODY, which is why one set of rules has two
//! entry points. Everything about the payload is shared and defined once; what differs
//! is the DESTINATION COORDINATE the body carries. The human door's body may name a
//! reply target inside the conversation it is already posting to. The agent door's
//! body must carry an absolute address, and carry it REQUIRED, because an agent's
//! credential says where it lives and never where its words go. One shape for both
//! costs a wrong destination in both directions: a reply silently flattened onto the
//! main line, or an address taken from whatever coordinate happened to be at hand.
//!
//! HOW THE SHAPE IS DECIDED, in the governance model the mounted write faces share
//! (frozen `#2236`): a typed Serde DTO owns structure — a missing required field, a
//! wrong-typed one, a present `null` where the schema said optional-not-nullable — and
//! a `garde` stateless rule owns the field predicates that need no cross-field context.
//! What is left — normalization, the domain-id parses, the attachment union's label
//! trim and uniqueness, and the "this send says something" refine — is the DTO's
//! [`TryFrom`], because each reads more than one field or hands a value to its domain
//! authority. Any of those failing is ONE semantic refusal ([`PayloadRefusal::Invalid`],
//! rendered `422 {"error":"invalid input"}`); the door never publishes a validation-issue
//! schema, and the order the rules fire in is not a wire contract. The content cap is the
//! sole exception that keeps its own answer, because the two doors word it differently.

use garde::Validate;
use serde::Deserialize;
use silverhand_backend_core::conversation_messaging::messaging::collaboration::message_options::{
    MessageOptions, OptionMode,
};
use silverhand_backend_core::conversation_messaging::messaging::{
    AttachmentId, ClientMessageId, MessageContent, MessageId,
    attachments::{AttachmentLabel, AttachmentRef},
};
use silverhand_backend_core::foundation::cli_invoke_audit::CliStaleResolutionReason;

use crate::foundation::wire_patch::RequiredPatchField;

/// The most attachments one message may reference.
pub(crate) const MAX_MESSAGE_ATTACHMENTS: usize = 20;

/// A request whose own content is settled: within the cap, with a legal attachment
/// list, and carrying something to say. No principal has been considered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedSendPayload {
    pub(crate) client_message_id: Option<ClientMessageId>,
    pub(crate) options: Option<MessageOptions>,
    content: MessageContent,
    attachments: Vec<AttachmentRef>,
}

impl ValidatedSendPayload {
    pub(crate) fn content(&self) -> &MessageContent {
        &self.content
    }

    pub(crate) fn attachments(&self) -> &[AttachmentRef] {
        &self.attachments
    }
}

/// A request that has passed shape and the refine but has NOT yet met the content cap:
/// STAGE ONE of closing a request, held apart from stage two so a door may run its own
/// rule between them.
///
/// It is deliberately opaque. Its fields are private, it has no public constructor, and
/// it exposes neither the raw text nor a [`ValidatedSendPayload`] — the ONLY way to
/// obtain it is [`refine`], and the ONLY thing that can be done with it is [`seal`]. Two
/// order mistakes are therefore unrepresentable rather than merely discouraged: a door
/// cannot reach the cap without having refined (there is no other producer of this type),
/// and it cannot forget the cap and send the text as-is (there is no accessor and no
/// `ValidatedSendPayload` that did not come out of `seal`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnsealedSendPayload {
    content: String,
    attachments: Vec<AttachmentRef>,
}

/// A settled HUMAN request: the shared payload plus the only destination this door can
/// express — a message in the same conversation to reply to.
///
/// The coordinate is `Option`, not a default: absent means the main line, and the
/// distinction is the difference between a reply and a top-level message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedHumanSend {
    client_message_id: Option<ClientMessageId>,
    unsealed: UnsealedSendPayload,
    reply_to: Option<MessageId>,
}

impl ValidatedHumanSend {
    pub(crate) fn reply_to(&self) -> Option<&MessageId> {
        self.reply_to.as_ref()
    }

    /// STAGE TWO for the human doors: meet the content cap and hand back the send-ready
    /// payload. The ordinary door calls this the moment it has admitted the body
    /// (admit → seal); the direct-pair door calls it only AFTER refusing a reply target,
    /// which is how ONE set of rules runs in the two orders the two doors need without
    /// either restating a rule. Both stages fail with the same [`PayloadRefusal`], so a
    /// door maps them to the wire uniformly.
    pub(crate) fn seal(self) -> Result<ValidatedSendPayload, PayloadRefusal> {
        let mut payload = seal(self.unsealed)?;
        payload.client_message_id = self.client_message_id;
        Ok(payload)
    }
}

/// A settled AGENT request: the shared payload, the address it named, and whether it
/// asked to skip the freshness guard.
///
/// The address is carried as the RAW STRING the caller wrote. Its grammar has exactly
/// one authority — the resolver behind [`super::adapters::TargetAccess`] — and restating
/// any part of it here would give the system two answers to "is this a legal address".
/// The DTO's only rule for it is `.min(1)`, so an empty string never reaches resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedAgentSend {
    payload: ValidatedSendPayload,
    target: String,
    freshness_bypass: bool,
    stale_resolution_reason: Option<CliStaleResolutionReason>,
}

impl ValidatedAgentSend {
    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    pub(crate) fn freshness_bypass(&self) -> bool {
        self.freshness_bypass
    }

    pub(crate) fn stale_resolution_reason(&self) -> Option<CliStaleResolutionReason> {
        self.stale_resolution_reason
    }

    pub(crate) fn into_payload(self) -> ValidatedSendPayload {
        self.payload
    }
}

/// Why admission refused, as the reason and not its rendering. Each door maps these
/// to its own wire words; the decision itself happens once.
///
/// TWO REASONS, not the old open-ended issue set. A structural or semantic fault of any
/// kind is [`Invalid`](Self::Invalid) — the door renders it the single shared
/// `422 {"error":"invalid input"}` and never enumerates what was wrong. The content cap
/// is its own variant only because the two doors word it differently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PayloadRefusal {
    /// The body is not a shape a send has, or a stateless rule or the refine refused it.
    /// One answer, no published reasons.
    Invalid,
    /// Legal shape, but the content is over the UTF-8 cap. The two doors word this
    /// one differently, which is why it is its own variant.
    ContentTooLarge { actual_bytes: usize },
}

/// ECMAScript's `WhiteSpace` and `LineTerminator`, which is what the reference's
/// `.trim()` removes.
///
/// NOT `char::is_whitespace`, and the difference is not academic: JavaScript trims
/// U+FEFF and leaves U+0085, while Rust does the exact opposite. Delegating to the
/// Unicode property would admit a body of nothing but a zero-width space AND refuse
/// one containing a legal next-line character — two wrong answers in opposite
/// directions, on the rule that decides whether a message says anything.
const ECMASCRIPT_WHITESPACE: &[char] = &[
    '\u{9}', '\u{A}', '\u{B}', '\u{C}', '\u{D}', '\u{20}', '\u{A0}', '\u{1680}', '\u{2000}',
    '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}',
    '\u{2009}', '\u{200A}', '\u{2028}', '\u{2029}', '\u{202F}', '\u{205F}', '\u{3000}', '\u{FEFF}',
];

/// The reference's `.trim()`, exactly.
pub(crate) fn ecmascript_trim(value: &str) -> &str {
    value.trim_matches(|character| ECMASCRIPT_WHITESPACE.contains(&character))
}

// ===========================================================================
// The typed closed envelopes. Serde owns structure; garde owns the stateless
// field predicates; the `TryFrom` below owns normalization and the refine.
// ===========================================================================

/// The stale-resolution reasons a SEND may carry. Three, not four: `obsolete` says
/// "those words are no longer worth writing", which is the one resolution that cannot
/// arrive attached to a message being written — the schema's `z.enum([...])`, so an
/// out-of-set value is a native decode fault rather than a hand-rolled rejection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SendStaleReason {
    Unaffected,
    Revised,
    ForceSend,
}

/// One member of the attachment union the reference accepts: a bare id string, or an
/// object carrying an id and an optional label.
///
/// `#[serde(untagged)]` is the union itself: a JSON string decodes to [`Bare`](Self::Bare),
/// an object to [`Labeled`](Self::Labeled), and anything else fails to match either arm —
/// a native decode fault. The object arm strips unknown members (no `deny_unknown_fields`),
/// matching the schema, and its `label` is optional-not-nullable so a present `null` is a
/// decode fault rather than a way to say "no label".
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
enum AttachmentRefInput {
    Bare(String),
    Labeled(LabeledAttachmentInput),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct LabeledAttachmentInput {
    id: String,
    #[serde(default)]
    label: RequiredPatchField<String>,
}

/// The HUMAN door's body: content, an optional reply target, and the attachment list.
///
/// `content` is required and any length — the type is the whole rule; the "says
/// something" decision is the refine, in [`TryFrom`]. `reply_to_message_id` is
/// optional-not-nullable (a present `null` is a decode fault) and its `.min(1)` is the
/// domain id's own rule, asked in [`TryFrom`]. `attachment_ids` defaults to empty, rejects
/// a present `null` natively (a `Vec` is not a `null`), and is bounded here.
#[derive(Debug, Deserialize, Validate)]
struct HumanSendBody {
    #[serde(default)]
    #[garde(skip)]
    client_message_id: RequiredPatchField<String>,
    #[garde(skip)]
    content: String,
    #[serde(default)]
    #[garde(skip)]
    reply_to_message_id: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(length(max = MAX_MESSAGE_ATTACHMENTS))]
    attachment_ids: Vec<AttachmentRefInput>,
}

/// The AGENT door's body: content, the required address, the stale-resolution telemetry,
/// the freshness escape hatch, and the attachment list.
///
/// `target` is required and `.min(1)`: an omitted or empty address is a client bug, because
/// an agent's credential says where it lives and never where its words go. The two stale
/// members and the bypass flag are optional-not-nullable; a present one is shape-checked
/// even though this slice carries the telemetry no further (a named gap, R5), because its
/// SHAPE is still part of what the door accepts.
#[derive(Debug, Deserialize, Validate)]
struct AgentSendBody {
    #[garde(skip)]
    content: String,
    #[garde(length(min = 1))]
    target: String,
    #[serde(default)]
    #[garde(custom(present_non_empty))]
    stale_event_id: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(skip)]
    stale_resolution_reason: RequiredPatchField<SendStaleReason>,
    #[serde(default)]
    #[garde(skip)]
    freshness_bypass: RequiredPatchField<bool>,
    #[serde(default)]
    #[garde(length(max = MAX_MESSAGE_ATTACHMENTS))]
    attachment_ids: Vec<AttachmentRefInput>,
    #[serde(default)]
    #[garde(skip)]
    options: RequiredPatchField<MessageOptionsInput>,
}

/// Answer options as the wire spells them; the grammar is the domain's.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct MessageOptionsInput {
    mode: String,
    items: Vec<MessageOptionItemInput>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct MessageOptionItemInput {
    id: String,
    label: String,
}

impl MessageOptionsInput {
    pub(crate) fn parse(self) -> Option<MessageOptions> {
        let mode = OptionMode::parse(&self.mode)?;
        MessageOptions::parse(
            mode,
            self.items.into_iter().map(|item| (item.id, item.label)),
        )
        .ok()
    }
}

/// A garde rule for an optional-not-nullable string that must be non-empty when present:
/// the `.min(1)` half of a `z.string().min(1).optional()` whose value the route drops.
/// Absent carries no value to bound and passes; a present empty string refuses.
fn present_non_empty(field: &RequiredPatchField<String>, _: &()) -> garde::Result {
    match field {
        RequiredPatchField::Value(value) if value.is_empty() => {
            Err(garde::Error::new("field must not be empty"))
        }
        _ => Ok(()),
    }
}

/// The HUMAN door's entry point: decode the body into its typed envelope, enforce the
/// stateless rules, then run the normalization and refine.
///
/// A body that could not be read as JSON at all arrives as `None` and is treated as an
/// empty document — the same substitution the reference makes before letting its schema
/// speak, so every required field is then missing and the request is refused rather than
/// collapsing into one unlocatable error. A present body that is not an object (a bare
/// number, array, string) fails the object decode and is equally [`PayloadRefusal::Invalid`].
///
/// The refine is done, the content cap is NOT: this returns a [`ValidatedHumanSend`] whose
/// [`seal`](ValidatedHumanSend::seal) is stage two, so a door may run its own rule between.
pub(crate) fn validate_human_payload(
    body: Option<serde_json::Value>,
) -> Result<ValidatedHumanSend, PayloadRefusal> {
    let raw: HumanSendBody = decode(body)?;
    raw.validate().map_err(|_| PayloadRefusal::Invalid)?;
    ValidatedHumanSend::try_from(raw)
}

/// The AGENT door's entry point: the mirror of [`validate_human_payload`] for the agent
/// envelope. It seals in the same step, because this door has no rule to insert between the
/// refine and the cap.
pub(crate) fn validate_agent_payload(
    body: Option<serde_json::Value>,
) -> Result<ValidatedAgentSend, PayloadRefusal> {
    let raw: AgentSendBody = decode(body)?;
    raw.validate().map_err(|_| PayloadRefusal::Invalid)?;
    ValidatedAgentSend::try_from(raw)
}

/// Decode a raw body into its typed envelope, substituting an empty object for a body that
/// could not be read. Any structural fault — missing field, wrong type, present `null`
/// where the schema forbids it — is the one semantic refusal.
///
/// The top-level object floor — rejecting a value-shaped JSON array that serde would
/// otherwise bind to the struct's fields positionally — is NOT hand-rolled here. It is the
/// shared [`ObjectOnly<T>`](crate::foundation::wire_admission::ObjectOnly) map-only wrapper from the
/// `ValidatedJson` governance anchor (#2246): decoding through it commits the value to a map
/// (a top-level array, scalar, or null becomes an "invalid type" fault) before delegating to
/// `T`'s own visitor, so every native `T` verdict is preserved. This door stays Value-first
/// to keep its access/validate/authority ordering, which means the body already passed
/// through `serde_json::Value` — duplicate keys were collapsed last-wins at that parse, a
/// route-order exception the anchor's slice path does not share.
fn decode<T: serde::de::DeserializeOwned>(
    body: Option<serde_json::Value>,
) -> Result<T, PayloadRefusal> {
    let value = body.unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    serde_json::from_value::<crate::foundation::wire_admission::ObjectOnly<T>>(value)
        .map(|crate::foundation::wire_admission::ObjectOnly(inner)| inner)
        .map_err(|_| PayloadRefusal::Invalid)
}

impl TryFrom<HumanSendBody> for ValidatedHumanSend {
    type Error = PayloadRefusal;

    fn try_from(raw: HumanSendBody) -> Result<Self, Self::Error> {
        let attachments = build_attachments(raw.attachment_ids)?;
        // A message in the SAME conversation. Which cluster that message belongs to is
        // resolved inside the write transaction, so this door carries the coordinate and
        // claims nothing about it. The `.min(1)` is the identifier's own rule — an empty
        // string is a refusal, not "no reply".
        let reply_to = match raw.reply_to_message_id {
            RequiredPatchField::Missing => None,
            RequiredPatchField::Value(raw) => {
                Some(MessageId::parse(raw).map_err(|_| PayloadRefusal::Invalid)?)
            }
        };
        let client_message_id = match raw.client_message_id {
            RequiredPatchField::Missing => None,
            RequiredPatchField::Value(id) => {
                Some(ClientMessageId::parse(id).map_err(|_| PayloadRefusal::Invalid)?)
            }
        };
        Ok(ValidatedHumanSend {
            client_message_id,
            unsealed: refine(raw.content, attachments)?,
            reply_to,
        })
    }
}

impl TryFrom<AgentSendBody> for ValidatedAgentSend {
    type Error = PayloadRefusal;

    fn try_from(raw: AgentSendBody) -> Result<Self, Self::Error> {
        let attachments = build_attachments(raw.attachment_ids)?;
        // Absent means "did not ask", which refuses the bypass exactly as `false` does.
        let freshness_bypass = matches!(raw.freshness_bypass, RequiredPatchField::Value(true));
        let stale_resolution_reason = if matches!(&raw.stale_event_id, RequiredPatchField::Value(_))
        {
            Some(match raw.stale_resolution_reason {
                RequiredPatchField::Value(SendStaleReason::Unaffected) => {
                    CliStaleResolutionReason::Unaffected
                }
                RequiredPatchField::Value(SendStaleReason::Revised) => {
                    CliStaleResolutionReason::Revised
                }
                RequiredPatchField::Value(SendStaleReason::ForceSend) => {
                    CliStaleResolutionReason::ForceSend
                }
                RequiredPatchField::Missing if freshness_bypass => {
                    CliStaleResolutionReason::ForceSend
                }
                RequiredPatchField::Missing => CliStaleResolutionReason::Unattributed,
            })
        } else {
            None
        };
        let options = match raw.options {
            RequiredPatchField::Missing => None,
            RequiredPatchField::Value(input) => Some(input.parse().ok_or(PayloadRefusal::Invalid)?),
        };
        // Options ask a question, so they need text to ask it with.
        if options.is_some() && ecmascript_trim(&raw.content).is_empty() {
            return Err(PayloadRefusal::Invalid);
        }
        let mut payload = seal(refine(raw.content, attachments)?)?;
        payload.options = options;
        Ok(ValidatedAgentSend {
            payload,
            target: raw.target,
            freshness_bypass,
            stale_resolution_reason,
        })
    }
}

/// The attachment union, normalized to the domain reference: each id parsed (emptiness is
/// the only rejection, through [`AttachmentId::parse`] rather than a second check here, so
/// `" "` is legal and refused later as an attachment nobody owns), each present label
/// trimmed with the reference's `.trim()` before the grammar applies, and no two labels
/// alike. All delegated to their single authorities; the bound was already enforced by
/// garde.
fn build_attachments(
    inputs: Vec<AttachmentRefInput>,
) -> Result<Vec<AttachmentRef>, PayloadRefusal> {
    let mut refs = Vec::with_capacity(inputs.len());
    for input in inputs {
        let (raw_id, raw_label) = match input {
            AttachmentRefInput::Bare(id) => (id, None),
            AttachmentRefInput::Labeled(LabeledAttachmentInput { id, label }) => (
                id,
                match label {
                    RequiredPatchField::Missing => None,
                    RequiredPatchField::Value(label) => Some(label),
                },
            ),
        };
        let id = AttachmentId::parse(raw_id).map_err(|_| PayloadRefusal::Invalid)?;
        let label = match raw_label {
            None => None,
            Some(raw) => {
                // Trimmed before the grammar applies, which admits `" Image #1 "`.
                let label =
                    AttachmentLabel::parse(ecmascript_trim(&raw)).ok_or(PayloadRefusal::Invalid)?;
                Some(label.to_string())
            }
        };
        refs.push(AttachmentRef { id, label });
    }
    if has_duplicate_label(&refs) {
        return Err(PayloadRefusal::Invalid);
    }
    Ok(refs)
}

/// Whether two members claim the same label. An unlabelled member is skipped, not the end
/// of the scan: the reference collects the labels that exist and compares those.
fn has_duplicate_label(refs: &[AttachmentRef]) -> bool {
    let mut seen: Vec<&str> = Vec::new();
    for reference in refs {
        let Some(label) = reference.label.as_deref() else {
            continue;
        };
        if seen.contains(&label) {
            return true;
        }
        seen.push(label);
    }
    false
}

/// STAGE ONE — the refine. It lives here rather than in `MessageContent`: an
/// attachment-only send is legal, so an empty string is legal CONTENT — the rule is
/// "this request says something", a fact about the request and not an invariant of the
/// value. In the domain type it would forbid a legal send. Produces the opaque
/// [`UnsealedSendPayload`]; the cap is stage two, and the type is what keeps them from
/// being run out of order or one of them skipped.
///
/// The order between this and the cap is a PRODUCT decision, which is why the two stages
/// are separable rather than fused: a body of 70KiB of spaces is answered "this says
/// nothing" ([`PayloadRefusal::Invalid`]), not "this is too big"
/// ([`PayloadRefusal::ContentTooLarge`]), because the refine runs before the cap.
fn refine(
    content: String,
    attachments: Vec<AttachmentRef>,
) -> Result<UnsealedSendPayload, PayloadRefusal> {
    if ecmascript_trim(&content).is_empty() && attachments.is_empty() {
        return Err(PayloadRefusal::Invalid);
    }
    Ok(UnsealedSendPayload {
        content,
        attachments,
    })
}

/// STAGE TWO — the cap. The domain type's own invariant, asked here rather than
/// restated: one comparison, in one place, and no way to build content that skipped it.
///
/// It is the SOLE producer of [`ValidatedSendPayload`], and it consumes a refined value
/// rather than a raw string, so a caller can neither reach the cap without having refined
/// nor obtain a send-ready payload any other way.
fn seal(unsealed: UnsealedSendPayload) -> Result<ValidatedSendPayload, PayloadRefusal> {
    let UnsealedSendPayload {
        content,
        attachments,
    } = unsealed;

    let content =
        MessageContent::parse(content).map_err(|too_long| PayloadRefusal::ContentTooLarge {
            actual_bytes: too_long.actual_bytes,
        })?;

    Ok(ValidatedSendPayload {
        client_message_id: None,
        options: None,
        content,
        attachments,
    })
}

#[cfg(test)]
mod tests {
    use silverhand_backend_core::conversation_messaging::messaging::MAX_MESSAGE_CONTENT_BYTES;

    use super::*;

    fn body(json: serde_json::Value) -> Option<serde_json::Value> {
        Some(json)
    }

    /// The rules BOTH doors share, reached through one of them. Which door asks is
    /// irrelevant to every assertion below: that is the point of one rule set.
    ///
    /// It composes the two stages in the ORDINARY door's order — refine, then seal —
    /// with nothing between, so a `ContentTooLarge` answer can only come after
    /// says-nothing has passed. A test that feeds input which is BOTH (70KiB of spaces)
    /// therefore reads the refine's answer through the real split, not a fused helper.
    fn admit(body: Option<serde_json::Value>) -> Result<ValidatedSendPayload, PayloadRefusal> {
        validate_human_payload(body)?.seal()
    }

    /// Reject as the one semantic refusal, never the cap.
    fn assert_invalid(refusal: PayloadRefusal) {
        assert_eq!(refusal, PayloadRefusal::Invalid);
    }

    /// The four readings a narrower rule gets wrong: a mixed list, a padded label, a bare
    /// id kept verbatim, and an unknown object member dropped.
    #[test]
    fn the_attachment_list_accepts_exactly_what_the_contract_accepts() {
        let mixed = admit(body(serde_json::json!({
            "content": "hi",
            "attachment_ids": ["att-1", {"id": "att-2"}, {"id": "att-3", "label": "Image #1"}]
        })))
        .unwrap();
        assert_eq!(mixed.attachments().len(), 3);
        assert_eq!(mixed.attachments()[2].label.as_deref(), Some("Image #1"));

        let padded = admit(body(serde_json::json!({
            "content": "hi",
            "attachment_ids": [{"id": "att-1", "label": " Image #1 "}]
        })))
        .unwrap();
        assert_eq!(padded.attachments()[0].label.as_deref(), Some("Image #1"));

        // A bare id is NOT trimmed.
        let blank = admit(body(
            serde_json::json!({"content": "hi", "attachment_ids": [" "]}),
        ))
        .unwrap();
        assert_eq!(blank.attachments()[0].id.as_str(), " ");

        // Unknown object members are dropped, not refused.
        assert!(
            admit(body(serde_json::json!({
                "content": "hi",
                "attachment_ids": [{"id": "att-1", "future_field": 7}]
            })))
            .is_ok()
        );
    }

    /// A member this stage cannot admit is a refusal, whatever the fault: a `null` entry,
    /// a wrong-typed id or label, a label that breaks the grammar, an empty id.
    #[test]
    fn a_bad_attachment_member_is_refused() {
        for entry in [
            serde_json::json!(null),
            serde_json::json!({"id": 7}),
            serde_json::json!({"id": "att-2", "label": 7}),
            serde_json::json!({"id": "att-2", "label": "nonsense"}),
            serde_json::json!(""),
        ] {
            let refusal = admit(body(serde_json::json!({
                "content": "hi",
                "attachment_ids": ["att-1", entry.clone()]
            })))
            .unwrap_err();
            assert_invalid(refusal);
        }
    }

    /// `.optional()` is not nullable, at either level. An absent list is a message with no
    /// attachments; an explicit `null` is the wrong type. An absent label is unlabelled; a
    /// `null` label is the wrong type.
    #[test]
    fn an_absent_member_is_legal_but_an_explicit_null_is_not() {
        assert!(
            admit(body(serde_json::json!({"content": "hi"})))
                .unwrap()
                .attachments()
                .is_empty()
        );
        assert_invalid(
            admit(body(
                serde_json::json!({"content": "hi", "attachment_ids": null}),
            ))
            .unwrap_err(),
        );

        let unlabelled = admit(body(
            serde_json::json!({"content": "hi", "attachment_ids": [{"id": "att-1"}]}),
        ))
        .unwrap();
        assert_eq!(unlabelled.attachments()[0].label, None);
        assert_invalid(
            admit(body(serde_json::json!({
                "content": "hi",
                "attachment_ids": [{"id": "att-1", "label": null}]
            })))
            .unwrap_err(),
        );
    }

    /// The bound and uniqueness are both rules ABOUT the list, and each is a refusal: over
    /// the count, or two members claiming one label.
    #[test]
    fn the_list_bound_and_label_uniqueness_are_enforced() {
        let over: Vec<serde_json::Value> = (0..MAX_MESSAGE_ATTACHMENTS + 1)
            .map(|index| serde_json::json!({"id": format!("att-{index}")}))
            .collect();
        assert_invalid(
            admit(body(
                serde_json::json!({"content": "hi", "attachment_ids": over}),
            ))
            .unwrap_err(),
        );

        // Exactly the bound, with distinct labels, is legal.
        let at_bound: Vec<serde_json::Value> = (0..MAX_MESSAGE_ATTACHMENTS)
            .map(|index| serde_json::json!({"id": "att", "label": format!("Image #{}", index + 1)}))
            .collect();
        assert_eq!(
            admit(body(
                serde_json::json!({"content": "hi", "attachment_ids": at_bound}),
            ))
            .unwrap()
            .attachments()
            .len(),
            MAX_MESSAGE_ATTACHMENTS
        );

        // Two members, one label — refused however the labels were padded.
        assert_invalid(
            admit(body(serde_json::json!({
                "content": "hi",
                "attachment_ids": [
                    {"id": "att-1", "label": "Image #1"},
                    {"id": "att-2", "label": " Image #1 "}
                ]
            })))
            .unwrap_err(),
        );
    }

    /// The trim is ECMAScript's, and it disagrees with Rust's in BOTH directions — on the
    /// content refine and on a label, the two places it decides admission.
    #[test]
    fn the_refine_trims_what_the_reference_trims() {
        // U+FEFF: JavaScript trims it, so this body says nothing. Rust's `trim` keeps it.
        assert_invalid(admit(body(serde_json::json!({"content": "\u{FEFF}"}))).unwrap_err());
        // U+0085: JavaScript does NOT trim it, so this body says something. Rust's `trim`
        // removes it and would refuse a legal send.
        assert!(admit(body(serde_json::json!({"content": "\u{85}"}))).is_ok());

        let feff = admit(body(serde_json::json!({
            "content": "hi",
            "attachment_ids": [{"id": "att-1", "label": "\u{FEFF}Image #1\u{FEFF}"}]
        })))
        .unwrap();
        assert_eq!(feff.attachments()[0].label.as_deref(), Some("Image #1"));
        // U+0085 stays part of the text, so this is not a legal label.
        assert!(
            admit(body(serde_json::json!({
                "content": "hi",
                "attachment_ids": [{"id": "att-1", "label": "\u{85}Image #1"}]
            })))
            .is_err()
        );
    }

    /// The refine belongs to the request, not to the content value: a whitespace-only body
    /// says nothing, an attachment-only body says something with empty content, and the
    /// content value itself keeps no such rule.
    #[test]
    fn a_send_must_say_something_but_the_content_value_keeps_no_such_rule() {
        assert_invalid(admit(body(serde_json::json!({"content": "   "}))).unwrap_err());

        let attachment_only = admit(body(
            serde_json::json!({"content": "", "attachment_ids": ["att-1"]}),
        ))
        .unwrap();
        assert_eq!(attachment_only.content().as_str(), "");

        assert!(MessageContent::parse("").is_ok());
    }

    /// THE REFINE IS ANSWERED BEFORE THE CONTENT CAP, because in the reference the cap
    /// lives OUTSIDE the schema. The two stages keep distinct refusals, so the order is
    /// read back through them: oversized-and-silent is `Invalid` (the refine won),
    /// oversized-with-real-text is `ContentTooLarge` (the cap is the only fault left).
    #[test]
    fn the_refine_is_answered_before_the_content_cap() {
        let silent = " ".repeat(MAX_MESSAGE_CONTENT_BYTES + 1);
        assert_invalid(admit(body(serde_json::json!({"content": silent}))).unwrap_err());

        let over = "a".repeat(MAX_MESSAGE_CONTENT_BYTES + 1);
        assert_eq!(
            admit(body(serde_json::json!({"content": over}))).unwrap_err(),
            PayloadRefusal::ContentTooLarge {
                actual_bytes: MAX_MESSAGE_CONTENT_BYTES + 1
            }
        );
    }

    /// A document that is not an object, a body that could not be read at all, and an empty
    /// object all refuse the same way — the schema has required fields none of them carry.
    ///
    /// A value-shaped array (`["hi"]`, whose element would bind positionally to `content`)
    /// is deliberately NOT here: the top-level object floor that refuses it is the shared
    /// `ValidatedJson` anchor (#2246), and the field-order-matching array → structural
    /// reject cell lands with that consumption rather than a route-local guard.
    #[test]
    fn a_body_that_is_not_a_send_shape_is_refused() {
        for root in [
            serde_json::json!(7),
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!("x"),
            serde_json::json!(true),
            serde_json::json!({}),
        ] {
            assert_invalid(admit(body(root.clone())).unwrap_err());
            assert_invalid(validate_agent_payload(body(root.clone())).unwrap_err());
        }
        assert_invalid(admit(None).unwrap_err());
        assert_invalid(validate_agent_payload(None).unwrap_err());
    }

    /// EACH DOOR READS ONLY ITS OWN DESTINATION FIELD, and a member the other door owns is
    /// an unknown one — stripped. The falsifier for one shared shape: a single schema would
    /// either demand `target` from a person's client or accept an agent send with no
    /// address at all.
    #[test]
    fn a_door_reads_its_own_destination_field_and_ignores_the_other_doors() {
        // The human door has no `target` rule; the member is dropped.
        let human = validate_human_payload(body(serde_json::json!({
            "content": "hi",
            "target": "#conv:elsewhere"
        })))
        .unwrap();
        assert_eq!(human.reply_to(), None);

        // The agent door has no `reply_to_message_id` rule, and still requires its address.
        assert_invalid(
            validate_agent_payload(body(serde_json::json!({
                "content": "hi",
                "reply_to_message_id": "msg-1"
            })))
            .unwrap_err(),
        );
        let agent = validate_agent_payload(body(serde_json::json!({
            "content": "hi",
            "target": "#conv:elsewhere",
            "reply_to_message_id": "msg-1"
        })))
        .unwrap();
        assert_eq!(agent.target(), "#conv:elsewhere");
    }

    /// The agent door's telemetry members are shape-checked and reduced to a closed audit
    /// fact without retaining the stale-event id. A malformed one is a refusal, not a value
    /// quietly dropped.
    #[test]
    fn the_stale_resolution_members_are_shape_checked_where_they_arrive() {
        let legal = |extra: serde_json::Value| {
            let mut object = serde_json::json!({"content": "hi", "target": "#conv:elsewhere"});
            let map = object.as_object_mut().unwrap();
            for (key, value) in extra.as_object().unwrap() {
                map.insert(key.clone(), value.clone());
            }
            body(object)
        };

        for (extra, expected_reason, expected_bypass) in [
            (
                serde_json::json!({
                    "stale_event_id": "evt-1",
                    "stale_resolution_reason": "revised"
                }),
                Some(CliStaleResolutionReason::Revised),
                false,
            ),
            (
                serde_json::json!({
                    "stale_event_id": "evt-1",
                    "freshness_bypass": true
                }),
                Some(CliStaleResolutionReason::ForceSend),
                true,
            ),
            (
                serde_json::json!({ "stale_event_id": "evt-1" }),
                Some(CliStaleResolutionReason::Unattributed),
                false,
            ),
            (serde_json::json!({ "freshness_bypass": true }), None, true),
            (
                serde_json::json!({ "stale_resolution_reason": "revised" }),
                None,
                false,
            ),
        ] {
            let admitted = validate_agent_payload(legal(extra)).unwrap();
            assert_eq!(admitted.stale_resolution_reason(), expected_reason);
            assert_eq!(admitted.freshness_bypass(), expected_bypass);
        }

        for extra in [
            serde_json::json!({"stale_event_id": ""}),
            serde_json::json!({"stale_event_id": 7}),
            // `obsolete` resolves a refusal by abandoning the message, so it is not one of
            // the three a SEND may carry.
            serde_json::json!({"stale_resolution_reason": "obsolete"}),
            serde_json::json!({"stale_resolution_reason": 7}),
            serde_json::json!({"freshness_bypass": "yes"}),
        ] {
            assert_invalid(validate_agent_payload(legal(extra.clone())).unwrap_err());
        }
    }

    /// EVERY optional member is optional-not-nullable, so a present `null` is a refusal at
    /// each door, never the "missing" arm. The frozen tri-state's contract, read at the two
    /// entry points that consume it.
    #[test]
    fn a_present_null_on_an_optional_member_is_refused() {
        for null_member in [
            serde_json::json!({"content": "hi", "reply_to_message_id": null}),
            serde_json::json!({"content": "hi", "attachment_ids": null}),
        ] {
            assert_invalid(validate_human_payload(body(null_member.clone())).unwrap_err());
        }
        for null_member in [
            serde_json::json!({"content": "hi", "target": "t", "stale_event_id": null}),
            serde_json::json!({"content": "hi", "target": "t", "stale_resolution_reason": null}),
            serde_json::json!({"content": "hi", "target": "t", "freshness_bypass": null}),
            serde_json::json!({"content": "hi", "target": "t", "attachment_ids": null}),
        ] {
            assert_invalid(validate_agent_payload(body(null_member.clone())).unwrap_err());
        }
    }

    /// The reply coordinate's `.min(1)` is the identifier's own rule, so an empty string is
    /// a refusal rather than "no reply", a `null` is a refusal, and a real id is carried.
    #[test]
    fn an_empty_reply_target_is_a_refusal_rather_than_the_main_line() {
        assert_invalid(
            validate_human_payload(body(
                serde_json::json!({"content": "hi", "reply_to_message_id": ""}),
            ))
            .unwrap_err(),
        );
        assert_invalid(
            validate_human_payload(body(
                serde_json::json!({"content": "hi", "reply_to_message_id": null}),
            ))
            .unwrap_err(),
        );
        assert_eq!(
            validate_human_payload(body(
                serde_json::json!({"content": "hi", "reply_to_message_id": "msg-1"})
            ))
            .unwrap()
            .reply_to()
            .map(MessageId::as_str),
            Some("msg-1")
        );
    }

    /// The agent door requires an address, and empty or absent is the same refusal. The
    /// bypass flag is a runtime's to set: present-true asks, absent or false does not.
    #[test]
    fn the_agent_door_requires_an_address_and_reads_its_own_bypass() {
        for missing in [
            serde_json::json!({"content": "hi"}),
            serde_json::json!({"content": "hi", "target": ""}),
        ] {
            assert_invalid(validate_agent_payload(body(missing.clone())).unwrap_err());
        }

        assert!(
            validate_agent_payload(body(serde_json::json!({
                "content": "hi",
                "target": "#conv:elsewhere",
                "freshness_bypass": true
            })))
            .unwrap()
            .freshness_bypass()
        );
        assert!(
            !validate_agent_payload(body(
                serde_json::json!({"content": "hi", "target": "#conv:elsewhere"})
            ))
            .unwrap()
            .freshness_bypass()
        );
    }
}
