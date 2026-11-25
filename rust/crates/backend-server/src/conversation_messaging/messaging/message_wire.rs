//! The one projection of a committed message onto the wire, shared by every door and
//! frame that must show the same row.
//!
//! WHY THIS IS ITS OWN MODULE, not a pair of private helpers in [`super::ack`]: the
//! person's acknowledgement ([`super::ack::send_ack`]) and the conversation-scope
//! `message` broadcast frame (the post-commit
//! [`silverhand_backend_core::conversation_messaging::messaging::post_commit::CommittedMessagePublication`]
//! projector) are the SAME row seen by two audiences. The renderer runs one idempotent
//! apply seam over both, so a second projection here would make the sender's own view
//! converge differently from everyone else's — the exact hazard the ACK door's contract
//! warns against. This is a SINGLE PROJECTOR AUTHORITY, invoked independently by both
//! audiences: the ACK door and the broadcast frame each call it and get an equal `Value`,
//! not one value physically placed twice. What forbids them from diverging is the three
//! together — this one projector, both audiences genuinely consuming it, and an exact
//! integration gate that locks the two whole JSON payloads projected from the same
//! committed fact
//! ([`crate::foundation::scope_event_transport`]'s `the_ack_is_the_conversation_frame_plus_a_sender_only_diagnostic`,
//! which drives the real broadcast door and the real ACK off one committed send, on both
//! the main-line and reply arms). A shared helper alone would not; it is the gate that
//! proves it.
//!
//! The ACK is a STRICT SUPERSET of this shape: it appends `unresolved_references`, which
//! has an audience of one and never rides the broadcast. This module owns exactly the
//! shared row and its reply sidecar — nothing sender-only, nothing path-specific.
//!
//! THREE DIFFERENT WAYS EMPTINESS IS SPELLED, and they are not interchangeable:
//! `metadata`/`attachments` are null together when a message carries none, an unlabelled
//! attachment OMITS `label` rather than sending null, and `reactions`/`linked_task` are a
//! structural null on a row inserted this instant.

use serde_json::{Map, Value, json};
use silverhand_backend_core::conversation_messaging::messaging::{
    CreatedMessage, FromKind, ThreadReplyFact, attachments::AttachmentSnapshot,
    collaboration::workspace_rename_messaging::BoundWorkspaceRenameMessage,
    mention_cc::CommittedMentionCopy,
};

use super::author_wire::author_dto;

/// The inserted row, as every reader of this conversation sees it — the ONE shape the
/// ACK and the broadcast frame both project from.
pub(crate) fn message_dto(message: &CreatedMessage) -> Value {
    let attachments = attachment_list(&message.attachments);
    let mut dto = json!({
        "id": message.id.as_str(),
        "conversation_id": message.conversation_id.as_str(),
        // RAW provenance, kept beside the resolved `author` rather than replaced by it: this is
        // what self-identification and permission logic read.
        "from_kind": from_kind_word(message.from_kind),
        "from_agent_id": optional_str(message.from_agent_id.as_ref().map(|id| id.as_str())),
        "from_user_id": optional_str(message.from_user_id.as_ref().map(|id| id.as_str())),
        // Null for a system message, which has no principal to present.
        "author": match &message.author {
            Some(author) => author_dto(author),
            None => Value::Null,
        },
        "content": message.content.as_str(),
        // ONE projected attachment value, placed twice. Unlike the cross-audience case, this
        // genuinely is a single `Value` handed to both members here, so `metadata` cannot
        // describe attachments the typed member does not; two calls producing equal JSON would
        // only look like a mirror.
        "metadata": wrapped_metadata(&attachments),
        "attachments": attachments,
        // STRUCTURALLY absent, not a read that failed: a row inserted this instant has no
        // reactions, and nothing has had the chance to link a task to it.
        "reactions": Value::Null,
        "seq": message.seq.get(),
        "created_at": message.created_at_ms,
        "thread_root_id": optional_str(message.thread_root_id.as_ref().map(|id| id.as_str())),
        "linked_task": Value::Null,
    });
    if let Some(id) = &message.client_message_id {
        dto["client_message_id"] = json!(id);
    }
    if let Some(options) = &message.options {
        let state = silverhand_backend_core::conversation_messaging::messaging::collaboration::message_options::MessageOptionsState::new(options.clone(), None);
        super::collaboration::message_options::attach_to_message(&mut dto, Some(&state));
    }
    dto
}

/// The authoritative deltas a reply produced, both read off the ONE committed cluster
/// projection so the count and the entry cannot describe different clusters. Present only
/// for a reply; a main-line send changed no cluster and carries no sidecar.
pub(crate) fn message_projections(reply: &ThreadReplyFact) -> Value {
    let entry = reply.list_entry();
    json!({
        "thread_root_reply_count": {
            "root_id": entry.root_id().as_str(),
            "reply_count": entry.reply_count(),
        },
        // The `{entry}` wrapper is the contract's, reserving room for sibling hints without a
        // wire break.
        "threads_list_entry": {
            "entry": {
                "root_message_id": entry.root_id().as_str(),
                "root_author": match entry.root_author() {
                    Some(author) => author_dto(author),
                    None => Value::Null,
                },
                "root_preview": entry.root_preview(),
                "root_created_at": entry.root_created_at_ms(),
                "root_seq": entry.root_seq().get(),
                "reply_count": entry.reply_count(),
                "last_reply_at": entry.last_reply_at_ms(),
                "last_reply_message_id": entry.last_reply_message_id().as_str(),
                "repliers": Value::Array(entry.repliers().iter().map(author_dto).collect()),
                "replier_total": entry.replier_total(),
            }
        },
    })
}

/// The `{message, projections?}` object both audiences carry: the send ACK (which
/// adds `unresolved_references` on top) and the conversation-scope `message` broadcast
/// frame (which carries this alone). This is the single projector authority
/// gate names — the ACK and the frame each invoke it independently, and
/// `the_ack_is_the_conversation_frame_plus_a_sender_only_diagnostic` proves the two whole
/// payloads equal from one committed send; it is not one value placed twice.
///
/// `projections` is ABSENT for a main-line send — spread-omitted on the wire, never
/// `null` and never `{}` — because a main-line send changed no cluster and an empty
/// sidecar would invite a client to apply a delta that does not exist.
pub(crate) fn message_event_payload(
    message: &CreatedMessage,
    reply: Option<&ThreadReplyFact>,
) -> Value {
    let mut payload = Map::new();
    payload.insert("message".to_owned(), message_dto(message));
    if let Some(reply) = reply {
        payload.insert("projections".to_owned(), message_projections(reply));
    }
    Value::Object(payload)
}

/// A committed mention-CC copy, projected as the `message` frame it is published to
/// the direct line as. It is a SYSTEM message, and its `metadata` is the flat
/// mention-CC envelope — `subtype` plus the denormalized `source_*` coordinates — not
/// the attachments wrapper [`message_dto`] builds. Everything else is the same fresh-row
/// contract: `author`, `attachments`, `reactions`, `thread_root_id`, and `linked_task`
/// are structurally null.
///
/// It is its own projector rather than a call into [`message_dto`] because the two
/// disagree on exactly one member — `metadata` — and that member is the whole reason a
/// copy exists: it is what lets a reader render the card without fetching a source
/// conversation they may not see. The `content` is the already-rendered wrap body the
/// store wrote; re-rendering it here would make the frame and the stored row two
/// projections of two different things.
pub(crate) fn copy_dto(copy: &CommittedMentionCopy) -> Value {
    let meta = &copy.metadata;
    json!({
        "id": copy.id.as_str(),
        "conversation_id": copy.conversation_id.as_str(),
        "from_kind": from_kind_word(copy.from_kind),
        "from_agent_id": Value::Null,
        "from_user_id": Value::Null,
        "author": Value::Null,
        "content": copy.content,
        "metadata": json!({
            "subtype": meta.subtype,
            "source_workspace_id": meta.source_workspace_id.as_str(),
            "source_workspace_name": meta.source_workspace_name,
            "source_conversation_id": meta.source_conversation_id.as_str(),
            "source_conversation_title": meta.source_conversation_title,
            "source_message_id": meta.source_message_id.as_str(),
            "source_from_kind": from_kind_word(meta.source_from_kind),
            "source_from_user_id": meta.source_from_user_id.as_str(),
            "source_message_content": meta.source_message_content,
        }),
        "attachments": Value::Null,
        "reactions": Value::Null,
        "seq": copy.seq.get(),
        "created_at": copy.created_at_ms,
        "thread_root_id": Value::Null,
        "linked_task": Value::Null,
    })
}

/// A committed workspace-rename system message, projected as the `message` frame it is
/// published to its conversation as. Like [`copy_dto`], it is a SYSTEM message whose
/// `metadata` is a FLAT rename envelope — `subtype`/`old_name`/`new_name`/`scope` — not
/// the attachments wrapper [`message_dto`] builds; and every principal-bearing member
/// (`author`, both `from_*` ids) plus `attachments`/`reactions`/`thread_root_id`/
/// `linked_task` is structurally null.
///
/// It is its own projector, not a call into [`message_dto`], for the same reason
/// [`copy_dto`] is: the two disagree on exactly one member, `metadata`, and that member
/// is the whole point of a rename card — it is what lets a reader render "renamed from X
/// to Y" without another fetch. The committed rename fact
/// ([`BoundWorkspaceRenameMessage`]) is a different type from [`CreatedMessage`] and
/// carries no `attachments`, so its structured metadata cannot flow through the
/// attachments wrapper. The `content` is the already-rendered body the store wrote; it is
/// not re-rendered here, so the frame and the stored row stay one projection of one fact.
pub(crate) fn rename_dto(message: &BoundWorkspaceRenameMessage) -> Value {
    let draft = message.draft();
    json!({
        "id": message.message().as_str(),
        "conversation_id": draft.conversation().as_str(),
        "from_kind": from_kind_word(FromKind::System),
        "from_agent_id": Value::Null,
        "from_user_id": Value::Null,
        "author": Value::Null,
        "content": draft.content(),
        "metadata": json!({
            "subtype": draft.subtype(),
            "old_name": draft.old_name(),
            "new_name": draft.new_name(),
            "scope": draft.scope(),
        }),
        "attachments": Value::Null,
        "reactions": Value::Null,
        "seq": message.seq().get(),
        "created_at": message.created_at().get(),
        "thread_root_id": Value::Null,
        "linked_task": Value::Null,
    })
}

pub(crate) fn from_kind_word(kind: FromKind) -> &'static str {
    match kind {
        FromKind::User => "user",
        FromKind::Agent => "agent",
        FromKind::System => "system",
    }
}

fn optional_str(value: Option<&str>) -> Value {
    match value {
        Some(value) => json!(value),
        None => Value::Null,
    }
}

/// The stored metadata, which on this path holds attachments and nothing else.
///
/// It takes the ALREADY PROJECTED list rather than the snapshots, so there is exactly one place
/// that turns a snapshot into JSON. Null when the list is — the same condition, read off the same
/// value, so the typed member cannot describe attachments the metadata does not.
fn wrapped_metadata(attachments: &Value) -> Value {
    if attachments.is_null() {
        return Value::Null;
    }
    json!({ "attachments": attachments })
}

pub(crate) fn attachment_list(attachments: &[AttachmentSnapshot]) -> Value {
    if attachments.is_empty() {
        return Value::Null;
    }
    snapshots(attachments)
}

fn snapshots(attachments: &[AttachmentSnapshot]) -> Value {
    Value::Array(
        attachments
            .iter()
            .map(|snapshot| {
                let mut object = Map::new();
                object.insert("id".to_owned(), json!(snapshot.id.as_str()));
                object.insert("filename".to_owned(), json!(snapshot.filename));
                object.insert("size".to_owned(), json!(snapshot.size));
                object.insert("content_type".to_owned(), json!(snapshot.content_type));
                // OMITTED when the sender assigned none, which is not the same wire fact as a
                // null label: the contract makes it optional so a bare-id caller's attachment
                // falls back to a positional label downstream.
                if let Some(label) = &snapshot.label {
                    object.insert("label".to_owned(), json!(label.to_string()));
                }
                Value::Object(object)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use silverhand_backend_core::{
        conversation_messaging::messaging::{
            AttachmentId, MessageId, MessageSeq,
            attachments::{AttachmentLabel, AttachmentSnapshot},
            collaboration::workspace_rename_messaging::{
                CommittedWorkspaceRenameMessage, WorkspaceRenameMessageCreatedAt,
                WorkspaceRenameMessageDraft,
            },
        },
        foundation::identity::{ConversationId, WorkspaceId},
    };

    use super::*;

    fn snapshot(label: Option<&str>) -> AttachmentSnapshot {
        AttachmentSnapshot {
            id: AttachmentId::parse("att-1").unwrap(),
            filename: "notes.txt".to_owned(),
            size: 12,
            content_type: "text/plain".to_owned(),
            label: label.map(|text| AttachmentLabel::parse(text).expect("a legal label")),
        }
    }

    /// `metadata` IS NOT A FREE-FORM RECORD on this path, and may never become one.
    ///
    /// Both members come from the one `CreatedMessage.attachments`, so they live and die
    /// together and cannot describe different attachments. Asserted as whole JSON over a closed
    /// key set: the failure this guards is a second key quietly appearing in `metadata`, which a
    /// field probe would never notice.
    #[test]
    fn metadata_mirrors_the_attachments_and_carries_nothing_else() {
        assert_eq!(attachment_list(&[]), Value::Null);
        assert_eq!(wrapped_metadata(&Value::Null), Value::Null);

        let projected = attachment_list(&[snapshot(Some("Image #1"))]);
        assert_eq!(
            projected,
            json!([{
                "id": "att-1",
                "filename": "notes.txt",
                "size": 12,
                "content_type": "text/plain",
                "label": "Image #1",
            }])
        );
        // The metadata wraps the value the typed member IS, rather than re-projecting the
        // snapshots — so the two cannot describe different attachments. That property is
        // structural; this only reads it back.
        assert_eq!(
            wrapped_metadata(&projected),
            json!({ "attachments": projected })
        );
    }

    /// An unlabelled attachment OMITS `label`; it does not send null.
    ///
    /// A third spelling of emptiness on this surface, and the reason it matters: the contract
    /// makes the member optional so a bare-id caller's attachment falls back to a positional
    /// label downstream. A null would be a value that consumer has to special-case.
    #[test]
    fn an_unlabelled_attachment_omits_the_member() {
        let bare = attachment_list(&[snapshot(None)]);
        let entry = &bare[0];
        assert!(entry.get("label").is_none(), "{entry}");
        assert_eq!(
            entry,
            &json!({
                "id": "att-1",
                "filename": "notes.txt",
                "size": 12,
                "content_type": "text/plain",
            })
        );
    }

    fn bound_rename() -> silverhand_backend_core::conversation_messaging::messaging::collaboration::workspace_rename_messaging::BoundWorkspaceRenameMessage{
        let draft = WorkspaceRenameMessageDraft::for_names(
            WorkspaceId::parse("w").unwrap(),
            ConversationId::parse("active-a").unwrap(),
            "Old Workspace",
            "New Workspace",
        );
        CommittedWorkspaceRenameMessage::committed(
            draft.clone(),
            MessageId::parse("msg-1").unwrap(),
            MessageSeq::try_from(7).unwrap(),
            WorkspaceRenameMessageCreatedAt::new(77).unwrap(),
        )
        .bind(&draft)
        .unwrap()
    }

    /// The rename frame is a SYSTEM row with a FLAT rename `metadata` envelope, and its key
    /// set may never drift toward [`message_dto`]'s attachments wrapper.
    ///
    /// Asserted as whole JSON over a closed key set — top level and `metadata` both — because
    /// the failure this guards is a member quietly appearing or a `null` becoming a value a
    /// field probe would miss: `metadata` carrying an `attachments` key, `author` or a
    /// `from_*` id gaining a principal, or `content` being re-rendered rather than the stored
    /// body. `content` is exactly the draft's rendered body, not a second projection.
    #[test]
    fn rename_frame_is_a_flat_system_envelope_and_carries_nothing_else() {
        assert_eq!(
            rename_dto(&bound_rename()),
            json!({
                "id": "msg-1",
                "conversation_id": "active-a",
                "from_kind": "system",
                "from_agent_id": Value::Null,
                "from_user_id": Value::Null,
                "author": Value::Null,
                "content": WorkspaceRenameMessageDraft::for_names(
                    WorkspaceId::parse("w").unwrap(),
                    ConversationId::parse("active-a").unwrap(),
                    "Old Workspace",
                    "New Workspace",
                )
                .content(),
                "metadata": {
                    "subtype": "rename",
                    "old_name": "Old Workspace",
                    "new_name": "New Workspace",
                    "scope": "workspace",
                },
                "attachments": Value::Null,
                "reactions": Value::Null,
                "seq": 7,
                "created_at": 77,
                "thread_root_id": Value::Null,
                "linked_task": Value::Null,
            })
        );
    }
}
