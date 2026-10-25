//! `POST /internal/agents/{aid}/read-history` — a window of one conversation's
//! message history.
//!
//! Behind the runtime-credential perimeter (agent bound to `{aid}` first). The
//! target stream is resolved from the request `target` address (`message_history`
//! read intent — no pair allocation); a `/thread:` suffix scopes to the reply
//! cluster. Then the `read_messages` capability gates the resolved conversation.
//! The window read (before/after/around/head) is projected to the wire DTO with
//! per-message sender/snippet/metadata/attachments/reactions and, only when
//! `include_content`, the raw content.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, FromRequest, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::agent_read_history::{
        AgentReadHistoryRead, ReadHistoryAnchor, ReadHistoryMessage, ReadHistoryRefusal,
    },
    conversation_messaging::messaging::{
        AddressRefusal, DeliveryIntent, DirectPairAllocation, TargetAddress,
    },
    host_runtime::runtime_credential::RuntimePrincipal,
};

use crate::conversation_messaging::messaging::agent_capability_wire::capability_denied;
use crate::conversation_messaging::messaging::author_wire::author_dto;
use crate::foundation::http_error::{
    coded_error as coded, json_error as error, logged_500 as internal, logged_500 as internal_dyn,
};
use crate::foundation::wire_admission::ValidatedJson;
use crate::foundation::wire_coerce::CoercedNumber;
use crate::foundation::wire_patch::RequiredPatchField;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

pub const AGENT_READ_HISTORY_PATH: &str = "/internal/agents/{aid}/read-history";

pub struct AgentReadHistoryState<T, R> {
    pub resolver: T,
    pub reads: R,
}

struct Shared<T, R>(Arc<AgentReadHistoryState<T, R>>);
impl<T, R> Clone for Shared<T, R> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn agent_read_history_router<T, R>(state: AgentReadHistoryState<T, R>) -> Router
where
    T: TargetAddress + Send + Sync + 'static,
    R: AgentReadHistoryRead,
{
    Router::new()
        .route(AGENT_READ_HISTORY_PATH, post(read::<T, R>))
        .with_state(Shared(Arc::new(state)))
}

/// `AgentReadHistoryInputSchema`.
#[derive(Deserialize, garde::Validate)]
struct ReadHistoryBody {
    #[garde(skip)]
    target: String,
    #[garde(skip)]
    #[serde(default)]
    before_seq: CoercedNumber,
    #[garde(skip)]
    #[serde(default)]
    after_seq: CoercedNumber,
    #[garde(skip)]
    #[serde(default)]
    around_message_id: RequiredPatchField<String>,
    #[garde(skip)]
    #[serde(default)]
    include_content: RequiredPatchField<bool>,
    #[garde(skip)]
    #[serde(default)]
    include_system: RequiredPatchField<bool>,
    #[garde(skip)]
    #[serde(default)]
    limit: CoercedNumber,
}

async fn read<T, R>(
    State(state): State<Shared<T, R>>,
    Path(aid): Path<String>,
    principal: Option<Extension<RuntimePrincipal>>,
    request: Request,
) -> Response
where
    T: TargetAddress + Send + Sync + 'static,
    R: AgentReadHistoryRead,
{
    // IDENTITY FIRST (#25): bind `{aid}` before decoding the body.
    let principal = match bind_runtime_actor(principal.as_ref().map(|e| &e.0), &aid) {
        Ok(principal) => principal,
        Err(refusal) => return refusal.into_response(),
    };
    let agent = principal.agent().clone();
    let body = match ValidatedJson::<ReadHistoryBody>::from_request(request, &()).await {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    if body.target.is_empty() {
        return invalid_input();
    }
    // Numeric admission: `before_seq`/`after_seq`/`limit` coerce
    // per `z.coerce.number()` (numeric string/null/bool/singleton-array accepted) then
    // gate JS-safe int + domain. A present-but-uncoercible value is `invalid input`.
    if body.before_seq.is_invalid() || body.after_seq.is_invalid() || body.limit.is_invalid() {
        return invalid_input();
    }
    // At most one of before_seq / after_seq / around_message_id.
    let before_seq = body.before_seq.value();
    let after_seq = body.after_seq.value();
    let around = body.around_message_id.into_option();
    let anchor_count = usize::from(before_seq.is_some())
        + usize::from(after_seq.is_some())
        + usize::from(around.as_ref().is_some_and(|s| !s.is_empty()));
    if anchor_count > 1 {
        return invalid_input();
    }
    // Non-negative seqs (`z.coerce.number().int().nonnegative()`).
    if before_seq.is_some_and(|v| v < 0) || after_seq.is_some_and(|v| v < 0) {
        return invalid_input();
    }
    if around.as_ref().is_some_and(String::is_empty) {
        return invalid_input();
    }
    let include_content = body.include_content.into_option().unwrap_or(false);
    let include_system = body.include_system.into_option().unwrap_or(true);
    // `z.coerce.number().int().positive().max(100).default(50)`.
    let limit = match body.limit.value() {
        None => 50,
        Some(value) if (1..=100).contains(&value) => value,
        Some(_) => return invalid_input(),
    };
    let anchor = if let Some(before) = before_seq {
        ReadHistoryAnchor::BeforeSeq(before)
    } else if let Some(after) = after_seq {
        ReadHistoryAnchor::AfterSeq(after)
    } else if let Some(id) = around.clone().filter(|s| !s.is_empty()) {
        ReadHistoryAnchor::Around(id)
    } else {
        ReadHistoryAnchor::Head
    };

    let state = &state.0;

    // Resolve the target address (read intent; a direct pair is not allocated).
    let resolved = match state
        .resolver
        .resolve(
            &agent,
            &body.target,
            DirectPairAllocation::Forbidden,
            DeliveryIntent::Read,
        )
        .await
    {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(refusal)) => return address_refusal(refusal),
        Err(cause) => return internal_dyn(cause, "read-history target resolve failed"),
    };
    let conversation = resolved.conversation;
    let thread_root = resolved.thread_root.map(|root| root.as_str().to_owned());

    // The read authority authorizes IN-SNAPSHOT (existence + participation +
    // read_messages) and returns the window WITH the target's full coordinates; the
    // route no longer pre-checks placement/capability on a separate pooled read.
    let window = match state
        .reads
        .read_history(
            &agent,
            &conversation,
            thread_root.as_deref(),
            &anchor,
            include_system,
            include_content,
            limit,
        )
        .await
    {
        Ok(Ok(window)) => window,
        Ok(Err(ReadHistoryRefusal::AnchorNotFound)) => {
            return coded(
                StatusCode::NOT_FOUND,
                "message not found",
                "message_not_found",
            );
        }
        Ok(Err(ReadHistoryRefusal::ConversationNotFound)) => {
            return error(StatusCode::NOT_FOUND, "conversation not found");
        }
        Ok(Err(ReadHistoryRefusal::NotParticipant)) => {
            return coded(
                StatusCode::FORBIDDEN,
                "agent is not a member of this conversation",
                "not_conversation_member",
            );
        }
        Ok(Err(ReadHistoryRefusal::CapabilityDenied)) => {
            return capability_denied("read_messages");
        }
        Err(cause) => return internal(cause, "read-history read failed"),
    };

    let coords = &window.target;
    let target = TargetRef {
        conversation_id: conversation.as_str().to_owned(),
        conversation_placement_kind: coords.placement_kind.as_str().to_owned(),
        conversation_roster_policy: coords.roster_policy.as_str().to_owned(),
        channel_id: coords.channel_id.as_str().to_owned(),
        thread_root_id: thread_root.clone(),
        workspace_id: coords.workspace_id.as_ref().map(|w| w.as_str().to_owned()),
        title: coords.title.clone(),
    };
    let body = ReadHistoryResponse {
        messages: match window
            .messages
            .iter()
            .map(|m| message_dto(m, &target, include_content))
            .collect::<Result<Vec<_>, WireShapeViolation>>()
        {
            Ok(messages) => messages,
            // A sealed field with the wrong SHAPE (even from a malicious port impl) is
            // a fail-closed 500 at the wire boundary — never a wrong-shaped 200.
            Err(_) => {
                tracing::error!(
                    "read-history projection: a message carried a malformed metadata/attachments/reactions shape"
                );
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
                    .into_response();
            }
        },
        anchor: anchor_dto(&anchor),
        limit,
        include_content,
        include_system,
        has_more_before: window.has_more_before,
        has_more_after: window.has_more_after,
        target,
    };
    (StatusCode::OK, Json(body)).into_response()
}

fn anchor_dto(anchor: &ReadHistoryAnchor) -> serde_json::Value {
    match anchor {
        ReadHistoryAnchor::Head => serde_json::json!({ "type": "head" }),
        ReadHistoryAnchor::BeforeSeq(seq) => {
            serde_json::json!({ "type": "before_seq", "seq": seq })
        }
        ReadHistoryAnchor::AfterSeq(seq) => serde_json::json!({ "type": "after_seq", "seq": seq }),
        ReadHistoryAnchor::Around(id) => {
            serde_json::json!({ "type": "around_message_id", "message_id": id })
        }
    }
}

/// A sealed read-history field carried the wrong wire SHAPE. The route treats this
/// as a fail-closed 500: even a malicious `AgentReadHistoryRead`
/// port cannot emit a wrong-shaped `200`, because the wire projector validates the
/// three JSON carriers' shapes at the boundary regardless of the port.
#[derive(Debug)]
struct WireShapeViolation;

fn message_dto(
    message: &ReadHistoryMessage,
    target: &TargetRef,
    include_content: bool,
) -> Result<serde_json::Value, WireShapeViolation> {
    let sender = message
        .author
        .as_ref()
        .map(|author| sender_ref(&author_dto(author)));
    // metadata/attachments/reactions are the read authority's carriers, but their
    // wire SHAPE is validated HERE at the boundary: metadata is an object
    // or null; attachments and reactions are an array or null (TS emits null when
    // empty). Any other shape — from a malformed production row that slipped the
    // adapter, or a malicious port — fails the whole response closed.
    let metadata = validate_object_or_null(&message.metadata)?;
    let attachments = validate_attachments(&message.attachments)?;
    let reactions = validate_reactions(&message.reactions)?;
    // Every message in a read-history window shares the ONE target's placement
    // coordinates (the whole window is one conversation); `workspace_id` is null for
    // a DM pair, `channel_id` is the conversation id (#2).
    let mut dto = serde_json::json!({
        "message_ref": {
            "id": message.message_id,
            "workspace_id": target.workspace_id,
            "conversation_id": message.conversation_id,
            "conversation_placement_kind": target.conversation_placement_kind,
            "conversation_roster_policy": target.conversation_roster_policy,
            "channel_id": target.channel_id,
            "seq": message.seq,
            "created_at": message.created_at,
        },
        "sender": sender,
        "snippet": message.snippet,
        "metadata": metadata,
        "attachments": attachments,
        "reactions": reactions,
    });
    if include_content {
        dto.as_object_mut().unwrap().insert(
            "content".to_owned(),
            serde_json::Value::String(message.content.clone()),
        );
    }
    Ok(dto)
}

/// The metadata carrier must be a JSON object or `null`.
fn validate_object_or_null(
    value: &serde_json::Value,
) -> Result<serde_json::Value, WireShapeViolation> {
    if value.is_object() || value.is_null() {
        Ok(value.clone())
    } else {
        Err(WireShapeViolation)
    }
}

/// The attachments carrier: `null`, or an array whose EVERY item satisfies
/// `AttachmentSnapshotSchema` — `id`/`filename`/`content_type` strings,
/// `size` a number, and (if present) `label` the canonical `^(Image|File) #([1-9]
/// \d{0,3})$`. A non-array, or ANY item that is not an object / is missing a required
/// field / has a wrong type / carries a non-canonical label, fails the whole response
/// closed — even from a malicious port. On success the DTO is REBUILT from the
/// whitelisted keys ONLY (matching the Zod `.strip()` the TS schema
/// applies): a valid SUPERSET item carrying an extra key (e.g. `secret`) has that key
/// dropped, so no unmodelled field leaks onto the 200.
fn validate_attachments(
    value: &serde_json::Value,
) -> Result<serde_json::Value, WireShapeViolation> {
    let items = match value {
        serde_json::Value::Null => return Ok(serde_json::Value::Null),
        serde_json::Value::Array(items) => items,
        _ => return Err(WireShapeViolation),
    };
    let mut rebuilt = Vec::with_capacity(items.len());
    for item in items {
        let object = item.as_object().ok_or(WireShapeViolation)?;
        // Required string fields.
        let mut dto = serde_json::Map::new();
        for key in ["id", "filename", "content_type"] {
            let field = object.get(key).ok_or(WireShapeViolation)?;
            if !field.is_string() {
                return Err(WireShapeViolation);
            }
            dto.insert(key.to_owned(), field.clone());
        }
        // `size` any number.
        let size = object.get("size").ok_or(WireShapeViolation)?;
        if !size.is_number() {
            return Err(WireShapeViolation);
        }
        dto.insert("size".to_owned(), size.clone());
        // Optional label: if present it must be a string matching the canonical shape.
        if let Some(label) = object.get("label") {
            let label_str = label.as_str().ok_or(WireShapeViolation)?;
            if !is_canonical_label(label_str) {
                return Err(WireShapeViolation);
            }
            dto.insert("label".to_owned(), label.clone());
        }
        // Only the whitelisted keys are carried forward — any extra key is stripped.
        rebuilt.push(serde_json::Value::Object(dto));
    }
    Ok(serde_json::Value::Array(rebuilt))
}

/// The reactions carrier: `null`, or an array whose EVERY item satisfies
/// `ReactionAggregateDtoSchema` — `emoji` a string, `count` a positive
/// integer, `viewer_reacted` a boolean. `[{}]`, a `count <= 0`, or a non-integer count
/// fails the whole response closed. On success the DTO is REBUILT from the whitelisted
/// keys ONLY (matching Zod `.strip()`): a valid superset item's extra key
/// is dropped, so no unmodelled field leaks onto the 200.
fn validate_reactions(value: &serde_json::Value) -> Result<serde_json::Value, WireShapeViolation> {
    let items = match value {
        serde_json::Value::Null => return Ok(serde_json::Value::Null),
        serde_json::Value::Array(items) => items,
        _ => return Err(WireShapeViolation),
    };
    let mut rebuilt = Vec::with_capacity(items.len());
    for item in items {
        let object = item.as_object().ok_or(WireShapeViolation)?;
        let emoji = object.get("emoji").ok_or(WireShapeViolation)?;
        if !emoji.is_string() {
            return Err(WireShapeViolation);
        }
        let viewer_reacted = object.get("viewer_reacted").ok_or(WireShapeViolation)?;
        if !viewer_reacted.is_boolean() {
            return Err(WireShapeViolation);
        }
        // `count`: a POSITIVE integer.
        let count_value = object.get("count").ok_or(WireShapeViolation)?;
        let count = count_value.as_i64();
        if !count.is_some_and(|c| c > 0) {
            return Err(WireShapeViolation);
        }
        // Rebuild from the whitelist only — extra keys stripped.
        let mut dto = serde_json::Map::new();
        dto.insert("emoji".to_owned(), emoji.clone());
        dto.insert("viewer_reacted".to_owned(), viewer_reacted.clone());
        dto.insert("count".to_owned(), count_value.clone());
        rebuilt.push(serde_json::Value::Object(dto));
    }
    Ok(serde_json::Value::Array(rebuilt))
}

/// The shared canonical attachment-label authority `^(Image|File) #([1-9]\d{0,3})$`
/// (the wire projector rejects a non-canonical label; the adapter emits only canonical
/// formatted labels).
fn is_canonical_label(label: &str) -> bool {
    let Some(rest) = label
        .strip_prefix("Image #")
        .or_else(|| label.strip_prefix("File #"))
    else {
        return false;
    };
    let bytes = rest.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 4
        && bytes[0] != b'0'
        && bytes.iter().all(u8::is_ascii_digit)
}

fn sender_ref(author: &serde_json::Value) -> serde_json::Value {
    let status = author
        .get("status")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if status == serde_json::Value::String("unresolved".to_owned()) {
        serde_json::json!({
            "status": "unresolved",
            "kind": author.get("kind").cloned().unwrap_or(serde_json::Value::Null),
            "id": serde_json::Value::Null,
            "name": serde_json::Value::Null,
            "display_name": serde_json::Value::Null,
        })
    } else {
        serde_json::json!({
            "status": status,
            "kind": author.get("kind").cloned().unwrap_or(serde_json::Value::Null),
            "id": author.get("id").cloned().unwrap_or(serde_json::Value::Null),
            "name": author.get("name").cloned().unwrap_or(serde_json::Value::Null),
            "display_name": author.get("display_name").cloned().unwrap_or(serde_json::Value::Null),
        })
    }
}

/// `AgentReadHistoryDto`.
#[derive(Serialize)]
struct ReadHistoryResponse {
    target: TargetRef,
    anchor: serde_json::Value,
    limit: i64,
    include_content: bool,
    include_system: bool,
    messages: Vec<serde_json::Value>,
    has_more_before: bool,
    has_more_after: bool,
}

#[derive(Serialize)]
struct TargetRef {
    conversation_id: String,
    conversation_placement_kind: String,
    conversation_roster_policy: String,
    channel_id: String,
    thread_root_id: Option<String>,
    workspace_id: Option<String>,
    title: String,
}

fn invalid_input() -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({ "error": "invalid input" })),
    )
        .into_response()
}

fn address_refusal(refusal: AddressRefusal) -> Response {
    let (status, message, code) = match refusal {
        AddressRefusal::Malformed => (
            StatusCode::BAD_REQUEST,
            "target is not an address",
            "target_malformed",
        ),
        AddressRefusal::SelfPair => (
            StatusCode::BAD_REQUEST,
            "a direct message needs another participant",
            "self_pair_not_addressable",
        ),
        AddressRefusal::DirectMessageScope => (
            StatusCode::FORBIDDEN,
            "Conversation-scoped agents do not support DMs. Use a shared channel.",
            "direct_message_scope_forbidden",
        ),
        AddressRefusal::Unresolved => (
            StatusCode::NOT_FOUND,
            "target names no stream you can use",
            "target_unresolved",
        ),
        AddressRefusal::ThreadNotInTarget => (
            StatusCode::NOT_FOUND,
            "target names no reply cluster of that conversation",
            "thread_not_in_target",
        ),
    };
    coded(status, message, code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::agent_read_history::ReadHistoryMessage;

    fn target() -> TargetRef {
        TargetRef {
            conversation_id: "conv-1".to_owned(),
            conversation_placement_kind: "workspace".to_owned(),
            conversation_roster_policy: "open".to_owned(),
            channel_id: "conv-1".to_owned(),
            thread_root_id: None,
            workspace_id: Some("ws-1".to_owned()),
            title: "Chan".to_owned(),
        }
    }

    fn msg(
        metadata: serde_json::Value,
        attachments: serde_json::Value,
        reactions: serde_json::Value,
    ) -> ReadHistoryMessage {
        ReadHistoryMessage {
            message_id: "m1".to_owned(),
            conversation_id: "conv-1".to_owned(),
            seq: 1,
            created_at: 10,
            thread_root_id: None,
            author: None,
            snippet: "hi".to_owned(),
            metadata,
            attachments,
            reactions,
            content: String::new(),
        }
    }

    #[test]
    fn message_dto_serializes_the_sealed_values_verbatim_without_reparse() {
        // The DTO carries the read authority's SEALED
        // metadata/attachments/reactions verbatim — no `from_str().ok().unwrap_or(Null)`
        // re-parse that could downgrade a value. A metadata OBJECT, a NON-empty
        // attachments array, and a reactions array all round-trip exactly.
        let metadata = serde_json::json!({"subtype": "note", "k": 1});
        let attachments = serde_json::json!([{"id": "a1", "filename": "f", "size": 3, "content_type": "text/plain"}]);
        let reactions = serde_json::json!([{"emoji": "👍", "count": 2, "viewer_reacted": true}]);
        let dto = message_dto(
            &msg(metadata.clone(), attachments.clone(), reactions.clone()),
            &target(),
            false,
        )
        .expect("well-formed shapes project");
        assert_eq!(dto["metadata"], metadata);
        assert_eq!(dto["attachments"], attachments);
        assert_eq!(dto["reactions"], reactions);
    }

    #[test]
    fn null_metadata_attachments_and_reactions_remain_null_in_the_projection() {
        // Storage represents absent carriers as null; projection preserves them.
        let dto = message_dto(
            &msg(
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::Value::Null,
            ),
            &target(),
            false,
        )
        .expect("null carriers project");
        assert_eq!(dto["metadata"], serde_json::Value::Null);
        assert_eq!(dto["attachments"], serde_json::Value::Null);
        assert_eq!(dto["reactions"], serde_json::Value::Null);
    }

    #[test]
    fn message_projection_rejects_wrong_shaped_json_carriers() {
        // Each malformed top-level carrier is a projection error.
        // metadata that is not object|null.
        assert!(
            message_dto(
                &msg(
                    serde_json::json!(7),
                    serde_json::Value::Null,
                    serde_json::Value::Null
                ),
                &target(),
                false,
            )
            .is_err()
        );
        // attachments that is not array|null.
        assert!(
            message_dto(
                &msg(
                    serde_json::Value::Null,
                    serde_json::json!({}),
                    serde_json::Value::Null
                ),
                &target(),
                false,
            )
            .is_err()
        );
        // reactions that is not array|null.
        assert!(
            message_dto(
                &msg(
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    serde_json::json!("x")
                ),
                &target(),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn message_projection_rejects_malformed_attachment_and_reaction_items() {
        // Per-ITEM domain validation, not just top-level array shape.
        let bad_attachment_cases = [
            // a non-object element
            serde_json::json!([7]),
            // missing required content_type
            serde_json::json!([{"id": "a", "filename": "f", "size": 1}]),
            // wrong type: size not a number
            serde_json::json!([{"id": "a", "filename": "f", "size": "x", "content_type": "t"}]),
            // non-canonical label
            serde_json::json!([{"id": "a", "filename": "f", "size": 1, "content_type": "t", "label": "custom"}]),
        ];
        for bad in bad_attachment_cases {
            assert!(
                message_dto(
                    &msg(
                        serde_json::Value::Null,
                        bad.clone(),
                        serde_json::Value::Null
                    ),
                    &target(),
                    false
                )
                .is_err(),
                "attachment item {bad} must fail closed"
            );
        }
        // A well-formed attachment (with a canonical label) projects.
        assert!(
            message_dto(
                &msg(
                    serde_json::Value::Null,
                    serde_json::json!([{"id": "a", "filename": "f", "size": 1.5, "content_type": "t", "label": "Image #1"}]),
                    serde_json::Value::Null,
                ),
                &target(),
                false,
            )
            .is_ok()
        );

        let bad_reaction_cases = [
            // empty object — missing all fields
            serde_json::json!([{}]),
            // count <= 0
            serde_json::json!([{"emoji": "👍", "count": 0, "viewer_reacted": true}]),
            // count non-integer
            serde_json::json!([{"emoji": "👍", "count": 1.5, "viewer_reacted": true}]),
            // viewer_reacted not a boolean
            serde_json::json!([{"emoji": "👍", "count": 2, "viewer_reacted": "yes"}]),
        ];
        for bad in bad_reaction_cases {
            assert!(
                message_dto(
                    &msg(
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                        bad.clone()
                    ),
                    &target(),
                    false
                )
                .is_err(),
                "reaction item {bad} must fail closed"
            );
        }
        // A well-formed reaction projects.
        assert!(
            message_dto(
                &msg(
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    serde_json::json!([{"emoji": "👍", "count": 2, "viewer_reacted": true}]),
                ),
                &target(),
                false,
            )
            .is_ok()
        );
    }

    #[test]
    fn a_valid_superset_item_has_its_extra_keys_stripped_from_the_wire() {
        // The per-item validators REBUILD the DTO from the whitelisted keys
        // (matching the Zod `.strip()` the TS schema applies), so a VALID item carrying
        // an extra `secret` key does not leak it onto the 200 — the wire item carries
        // ONLY the modelled fields.
        let dto = message_dto(
            &msg(
                serde_json::Value::Null,
                serde_json::json!([{
                    "id": "a", "filename": "f", "size": 1, "content_type": "t",
                    "label": "Image #1", "secret": "leak", "size_hint": 99
                }]),
                serde_json::json!([{
                    "emoji": "👍", "count": 2, "viewer_reacted": true, "secret": "leak"
                }]),
            ),
            &target(),
            false,
        )
        .expect("a valid superset item still projects (extra keys are stripped, not rejected)");

        let attachment = &dto["attachments"][0];
        let mut attachment_keys: Vec<&str> = attachment
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        attachment_keys.sort_unstable();
        assert_eq!(
            attachment_keys,
            vec!["content_type", "filename", "id", "label", "size"],
            "attachment DTO carries ONLY the whitelisted keys — `secret`/`size_hint` stripped"
        );
        assert!(attachment.get("secret").is_none(), "no `secret` leaked");
        assert!(
            attachment.get("size_hint").is_none(),
            "no `size_hint` leaked"
        );

        let reaction = &dto["reactions"][0];
        let mut reaction_keys: Vec<&str> = reaction
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        reaction_keys.sort_unstable();
        assert_eq!(
            reaction_keys,
            vec!["count", "emoji", "viewer_reacted"],
            "reaction DTO carries ONLY the whitelisted keys — `secret` stripped"
        );
        assert!(reaction.get("secret").is_none(), "no `secret` leaked");
    }
}
