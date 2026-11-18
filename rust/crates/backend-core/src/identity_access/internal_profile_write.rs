//! The INTERNAL self-service profile write use case for `POST
//! /internal/agents/{aid}/profile` (and the internal avatar `POST`/`DELETE`) — the
//! #2226 orchestration bounded context that drives the blob store and the SINGLE
//! canonical `agents`-row writer
//! ([`crate::identity_access::agent_profile_write::AgentProfileWriteUnitOfWork`]) to apply a combined
//! `display_name` / `description` / avatar edit in ONE row unit-of-work, firing EXACTLY
//! ONE `agent_changed` announce and cleaning up any replaced/removed blob.
//!
//! ## Why a combined lifecycle, distinct from `agent_avatar_write`
//!
//! The public avatar routes ([`crate::identity_access::agent_avatar_write`]) own an avatar-ONLY lifecycle.
//! This internal capability edits text AND avatar in the SAME request, and the product
//! contract (ratified) is a SINGLE atomic row write with a SINGLE announce — never the
//! avatar-then-text two-write / two-announce sequence with a partial-commit window. That
//! cannot be expressed by composing the avatar-only use cases (each fires its own
//! announce), so the combined lifecycle lives here. It declares NO row port, NO edit
//! algebra, and NO persistence fact of its own: the canonical writer, the sealed
//! [`AdmittedAgentProfileEdit`] (minted for this capability ONLY by
//! [`admit_internal_profile_edit`]), and the full-column seal all live in
//! `agent_profile_write`. It shares that canonical writer with tuning and the public
//! avatar lifecycle — no second capability-specific writer exists.
//!
//! ## Put-then-write, with transition-scoped cleanup carried to the boundary
//!
//! An avatar `Set` puts the FRESH blob before the row write (its pointer must exist
//! before the row can name it); a `Clear`/`Leave` puts nothing. The decision is replayed
//! UNDER the write lock against the locked before-image, and the domain re-derives the
//! verdict + sealing intent from the witnessed before-image after the commit — a lying or
//! buggy adapter can neither mint a refusal nor launder an unwarranted write. On any
//! non-trustworthy-commit path AFTER a put, the fresh blob is a true orphan and is cleaned
//! best-effort with its [`BlobCleanup`] fate carried — EXCEPT a durable-but-untrustworthy
//! commit, where the UPDATE persisted and the fresh blob may be referenced, so it is
//! deliberately RETAINED. A trustworthy commit cleans the PRIOR blob ONLY on an avatar
//! TRANSITION (`Set` or `Clear` off a present prior); a text-only edit never touches the
//! avatar blob even when one is present.

use crate::{
    foundation::identity::AgentId,
    identity_access::agent_avatar::{
        AgentAvatarAssignment, AgentAvatarBlobPointer, AgentAvatarBlobStore, AgentAvatarRow,
        OwnedAgentAvatarBlob,
    },
    identity_access::agent_profile_write::{
        AdmittedAgentProfileEdit, AgentChangedAnnounce, AgentChangedSink, AgentProfileRowWrite,
        AgentProfileWriteError, AgentProfileWriteUnitOfWork, AgentRowBefore, FieldEdit,
        RequiredEdit, admit_internal_profile_edit, seal_agent_row,
    },
    identity_access::avatar::{AvatarContentType, AvatarStorageFailure, BlobCleanup},
    identity_access::avatar_design::AvatarDesign,
};

// ===========================================================================
// The request: the already-validated internal-profile edit.
// ===========================================================================

/// A validated internal-profile edit: the text columns as a two-valued
/// leave-or-set (`description`'s empty string is a legitimate `Set`, never a clear),
/// plus the avatar action. Minted by the internal-profile boundary — every value here
/// has already cleared admission (display-name shape, description length, avatar
/// size/MIME), and the caller guarantees at least one field is present. The core owns
/// the WRITE lifecycle, not the request algebra.
pub struct InternalProfileEdit {
    pub display_name: RequiredEdit<String>,
    pub description: RequiredEdit<String>,
    pub avatar: InternalAvatarEdit,
}

/// The avatar action of an internal-profile edit: leave it untouched, clear it, or set
/// it to an already-validated payload (its size/MIME cleared admission; this use case
/// puts the blob, it does not re-validate).
pub enum InternalAvatarEdit {
    Leave,
    Clear,
    Set {
        content_type: AvatarContentType,
        bytes: Vec<u8>,
        design: Option<AvatarDesign>,
    },
}

/// The avatar sub-plan carried past the (possible) blob put into the decision: `Set`
/// already holds the freshly-put pointer, so the decision and the post-commit replay
/// build the identical avatar arm.
#[derive(Clone)]
enum AvatarPlan {
    Leave,
    Clear,
    Set(AgentAvatarAssignment),
}

// ===========================================================================
// The committed effect and the closed outcome.
// ===========================================================================

/// A committed internal-profile write together with the fate of its post-commit
/// `agent_changed` announce AND its best-effort blob cleanup (the replaced blob on an
/// avatar set, the removed blob on an avatar clear, `NothingToClean` on a text-only
/// edit). Reachable only through [`into_parts`](Self::into_parts), so a boundary taking
/// the id for the wire re-read receives BOTH residues in the same move and cannot drop
/// them. It carries only the id: the wire DTO is composed by re-reading through the
/// canonical profile projector, never from this effect.
#[derive(Debug)]
#[must_use = "a committed internal-profile write carries an announce fate and a blob-cleanup fate that must be handled"]
pub struct AnnouncedInternalProfileWrite {
    agent_id: AgentId,
    announce: AgentChangedAnnounce,
    blob_cleanup: BlobCleanup,
}

impl AnnouncedInternalProfileWrite {
    pub fn into_parts(self) -> (AgentId, AgentChangedAnnounce, BlobCleanup) {
        (self.agent_id, self.announce, self.blob_cleanup)
    }
}

/// The closed outcome of [`update_internal_profile`]. A failure reached BEFORE the
/// durable commit (`NoSuchAgent`, an untrustworthy `Unchanged`, a pre-commit persistence
/// fault) leaves any fresh blob provably unreferenced, so it is cleaned and its
/// [`BlobCleanup`] fate carried. A failure on a DURABLE commit
/// ([`CommitUntrustworthy`](Self::CommitUntrustworthy)) cannot prove the fresh blob
/// unreferenced — the UPDATE persisted — so a fresh blob is deliberately RETAINED.
#[derive(Debug)]
#[must_use = "an internal-profile write outcome carries a committed effect, a blob-cleanup fate, or a retained-blob residue that must be handled"]
pub enum InternalProfileWriteOutcome {
    /// The row was written and the change announced. Carries the prior/removed blob's
    /// cleanup fate (`NothingToClean` for a text-only edit). → product `200` (DTO re-read).
    Committed(AnnouncedInternalProfileWrite),
    /// The only warranted no-op: an avatar clear on an already-absent avatar with no text
    /// edit. No write, no delete, no announce. → product `200` (current DTO re-read).
    NoOp { agent_id: AgentId },
    /// The row vanished before the write. Any fresh blob is orphaned and cleaned
    /// best-effort. → product `404`.
    NoSuchAgent { orphan_cleanup: BlobCleanup },
    /// The blob `put` itself failed; no row was written and no blob persisted. → the
    /// product's `avatar_storage_failed`.
    StorageFailed(AvatarStorageFailure),
    /// A pre-commit fault — a persistence error, or a warranted write the adapter
    /// reported `Unchanged`: no durable commit, so a fresh blob is a true orphan, cleaned
    /// best-effort with its fate carried. → safe-`500`.
    WriteFaulted {
        cause: AgentProfileWriteError,
        orphan_cleanup: BlobCleanup,
    },
    /// The row committed durably, yet the returned witness could not be trusted (a
    /// cross-subject before-image, an unwarranted write, or a full-column seal mismatch).
    /// A fresh blob (an avatar `Set`) is DELIBERATELY retained — deleting it could strand
    /// the committed row on a dangling pointer; `retained_fresh` is `None` for a
    /// text-only/clear edit that put nothing. → safe-`500`, no announce.
    CommitUntrustworthy {
        retained_fresh: Option<AgentAvatarBlobPointer>,
    },
}

// ===========================================================================
// The use case.
// ===========================================================================

/// Apply a combined internal-profile edit under ONE canonical row write and, on a
/// trustworthy commit, fire EXACTLY ONE `agent_changed` announce. `subject` MUST have
/// already cleared the route's self-pin gate; this use case owns only the (put →) write →
/// seal → cleanup → announce lifecycle.
pub async fn update_internal_profile<W, B, S>(
    writes: &W,
    blobs: &B,
    announce_sink: &S,
    subject: &AgentId,
    edit: InternalProfileEdit,
    now_ms: i64,
) -> InternalProfileWriteOutcome
where
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
{
    let InternalProfileEdit {
        display_name,
        description,
        avatar,
    } = edit;

    // An avatar SET puts the FRESH blob before the row write — a set always assigns, so
    // its pointer must exist before the row can name it. Clear/Leave put nothing. `fresh`
    // is the just-put pointer whose custody every non-trustworthy-commit path reasons about.
    let (avatar_plan, fresh) = match avatar {
        InternalAvatarEdit::Leave => (AvatarPlan::Leave, None),
        InternalAvatarEdit::Clear => (AvatarPlan::Clear, None),
        InternalAvatarEdit::Set {
            content_type,
            bytes,
            design,
        } => {
            let pointer = match blobs.put(subject, content_type, bytes).await {
                Ok(pointer) => pointer,
                Err(failure) => return InternalProfileWriteOutcome::StorageFailed(failure),
            };
            let assignment = AgentAvatarAssignment {
                pointer: pointer.clone(),
                content_type,
                design,
            };
            (AvatarPlan::Set(assignment), Some(pointer))
        }
    };

    // The decision is replayed UNDER the write lock against the locked before-image; the
    // domain re-derives the identical intent below to seal the commit, so the adapter
    // never hands over an intent of its own.
    let decide_display_name = display_name.clone();
    let decide_description = description.clone();
    let decide_plan = avatar_plan.clone();
    let write = writes
        .write_agent_row(
            subject,
            move |before| {
                plan_internal_edit(
                    &decide_display_name,
                    &decide_description,
                    &decide_plan,
                    before,
                )
            },
            now_ms,
        )
        .await;

    let write = match write {
        Ok(write) => write,
        Err(failure) => {
            // Pre-commit persistence fault: no durable write, so a fresh blob is a true
            // orphan — clean it best-effort and carry its fate.
            let orphan_cleanup = cleanup_fresh(blobs, subject, fresh.as_ref()).await;
            return InternalProfileWriteOutcome::WriteFaulted {
                cause: AgentProfileWriteError::Persistence(failure),
                orphan_cleanup,
            };
        }
    };

    match write {
        AgentProfileRowWrite::NoSuchAgent => {
            let orphan_cleanup = cleanup_fresh(blobs, subject, fresh.as_ref()).await;
            InternalProfileWriteOutcome::NoSuchAgent { orphan_cleanup }
        }
        AgentProfileRowWrite::Unchanged { before } => {
            // Bind the witness to the subject before trusting its state, then replay the
            // rule: `None` is the sole warranted no-op (an avatar clear on an absent
            // avatar with no text); anything the rule would WRITE means a warranted write
            // was silently withheld — untrustworthy, and (no durable commit) a fresh blob
            // is a true orphan.
            let warranted_no_op = before.agent_id == *subject
                && plan_internal_edit(&display_name, &description, &avatar_plan, &before).is_none();
            if warranted_no_op {
                InternalProfileWriteOutcome::NoOp {
                    agent_id: before.agent_id,
                }
            } else {
                let orphan_cleanup = cleanup_fresh(blobs, subject, fresh.as_ref()).await;
                InternalProfileWriteOutcome::WriteFaulted {
                    cause: AgentProfileWriteError::UntrustworthyWriteFact,
                    orphan_cleanup,
                }
            }
        }
        AgentProfileRowWrite::Committed { before, row } => {
            // Bind the witness, re-derive the admitted intent by replaying the rule, and
            // seal the FULL after-image before trusting the commit. On ANY failure the
            // durable UPDATE persisted, so a fresh blob may be referenced by the row: it
            // is RETAINED (never deleted), never announced.
            let admitted = if before.agent_id == *subject {
                plan_internal_edit(&display_name, &description, &avatar_plan, &before)
            } else {
                None
            };
            let admitted = match admitted {
                Some(admitted)
                    if seal_agent_row(subject, &before, &admitted, &row, now_ms).is_ok() =>
                {
                    admitted
                }
                _ => {
                    return InternalProfileWriteOutcome::CommitUntrustworthy {
                        retained_fresh: fresh,
                    };
                }
            };
            // Trustworthy commit. Prior-blob cleanup is derived from the sealed avatar arm
            // + the before-image: a text-only edit (avatar `Leave`) NEVER touches the
            // avatar blob even when one is present; an avatar TRANSITION (`Set`/`Clear`)
            // cleans the present prior blob (a first upload off `Absent` cleans nothing).
            let blob_cleanup = match (admitted.avatar(), &before.avatar) {
                (FieldEdit::Leave, _) => BlobCleanup::NothingToClean,
                (_, AgentAvatarRow::Present { pointer: prior, .. }) => {
                    cleanup_agent_blob(blobs, subject, prior).await
                }
                (_, AgentAvatarRow::Absent) => BlobCleanup::NothingToClean,
            };
            let announce = announce_agent_changed(announce_sink, subject).await;
            InternalProfileWriteOutcome::Committed(AnnouncedInternalProfileWrite {
                agent_id: row.id,
                announce,
                blob_cleanup,
            })
        }
    }
}

/// Build the sealed edit for this write from the request + the locked before-image, or
/// `None` when nothing is warranted. PURE and state-dependent, so it is called both as
/// the write decision (under the lock) and as the post-commit replay (against the SAME
/// witnessed before-image) — the two always agree. A clear on an already-absent avatar
/// resolves to `Leave` (not a transition), so it neither forces a write nor a cleanup on
/// its own; only a present text or a real avatar transition warrants the write.
fn plan_internal_edit(
    display_name: &RequiredEdit<String>,
    description: &RequiredEdit<String>,
    avatar: &AvatarPlan,
    before: &AgentRowBefore,
) -> Option<AdmittedAgentProfileEdit> {
    let avatar_arm = match avatar {
        AvatarPlan::Leave => FieldEdit::Leave,
        AvatarPlan::Set(assignment) => FieldEdit::Set(assignment.clone()),
        AvatarPlan::Clear => match &before.avatar {
            AgentAvatarRow::Present { .. } => FieldEdit::Clear,
            AgentAvatarRow::Absent => FieldEdit::Leave,
        },
    };
    let writes_text =
        matches!(display_name, RequiredEdit::Set(_)) || matches!(description, RequiredEdit::Set(_));
    let writes_avatar = avatar_arm.is_present();
    if writes_text || writes_avatar {
        Some(admit_internal_profile_edit(
            display_name.clone(),
            description.clone(),
            avatar_arm,
        ))
    } else {
        None
    }
}

// ===========================================================================
// Shared effect plumbing (this bounded context's own; the avatar lifecycle's copies
// are private to that module).
// ===========================================================================

/// Clean a just-put FRESH blob orphan, if one was put. The `owner` is the write subject,
/// so the delete rides an [`OwnedAgentAvatarBlob`]: a pointer cross-wired to another
/// agent's key is refused by the adapter rather than deleting that agent's blob.
async fn cleanup_fresh<B>(
    blobs: &B,
    owner: &AgentId,
    fresh: Option<&AgentAvatarBlobPointer>,
) -> BlobCleanup
where
    B: AgentAvatarBlobStore,
{
    match fresh {
        Some(pointer) => cleanup_agent_blob(blobs, owner, pointer).await,
        None => BlobCleanup::NothingToClean,
    }
}

/// Run the best-effort post-commit blob delete and fold its result into a
/// [`BlobCleanup`] (a failure is carried, never propagated — the DB pointer is already
/// authoritative). The `owner` binds the delete to the write subject's custody.
async fn cleanup_agent_blob<B>(
    blobs: &B,
    owner: &AgentId,
    pointer: &AgentAvatarBlobPointer,
) -> BlobCleanup
where
    B: AgentAvatarBlobStore,
{
    let owned = OwnedAgentAvatarBlob::new(owner.clone(), pointer.clone());
    match blobs.delete(&owned).await {
        Ok(()) => BlobCleanup::Deleted,
        Err(failure) => BlobCleanup::Failed(failure),
    }
}

/// Fire the changed-only `agent_changed` announce and fold its result into the carried
/// value (a failed hint never downgrades the committed write).
async fn announce_agent_changed<S>(sink: &S, agent: &AgentId) -> AgentChangedAnnounce
where
    S: AgentChangedSink,
{
    match sink.agent_changed(agent).await {
        Ok(()) => AgentChangedAnnounce::Delivered,
        Err(failure) => AgentChangedAnnounce::Failed(failure),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        foundation::persistence::PersistenceFailure,
        identity_access::agent_avatar::AgentAvatarBlobRead,
        identity_access::agent_profile_write::{
            AgentChangedAnnounceFailure, AgentTuningContext, CommittedAgentRow,
        },
        identity_access::agents::runtime::RuntimeKind,
        identity_access::avatar::AvatarBlobReadOutcome,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use thiserror::Error;

    const NOW: i64 = 9_000;
    /// The pointer the fake blob store mints on its FIRST `put`.
    const FRESH: &str = "agents/mint/blob-0";

    fn agent() -> AgentId {
        AgentId::parse("agent-1").unwrap()
    }

    fn ptr(raw: &str) -> AgentAvatarBlobPointer {
        AgentAvatarBlobPointer::parse(raw).unwrap()
    }

    // ---- fakes ----

    /// A fake blob store: `put` mints a deterministic sequential pointer and counts puts;
    /// `delete` records the (owner, pointer) it was asked to delete and can be made to
    /// fail. Recording the OWNER lets a test prove the delete rode an owner-bound blob.
    #[derive(Clone)]
    struct FakeBlobs {
        puts: Arc<AtomicUsize>,
        next: Arc<AtomicUsize>,
        deletes: Arc<Mutex<Vec<(AgentId, String)>>>,
        fail_delete: bool,
    }

    impl FakeBlobs {
        fn new(fail_delete: bool) -> Self {
            Self {
                puts: Arc::new(AtomicUsize::new(0)),
                next: Arc::new(AtomicUsize::new(0)),
                deletes: Arc::new(Mutex::new(Vec::new())),
                fail_delete,
            }
        }
        fn puts(&self) -> usize {
            self.puts.load(Ordering::SeqCst)
        }
        fn deleted(&self) -> Vec<(AgentId, String)> {
            self.deletes.lock().unwrap().clone()
        }
        fn deleted_pointers(&self) -> Vec<String> {
            self.deleted().into_iter().map(|(_, p)| p).collect()
        }
    }

    impl AgentAvatarBlobRead for FakeBlobs {
        async fn read(
            &self,
            _blob: &OwnedAgentAvatarBlob,
        ) -> Result<AvatarBlobReadOutcome, AvatarStorageFailure> {
            Ok(AvatarBlobReadOutcome::Missing)
        }
    }

    impl AgentAvatarBlobStore for FakeBlobs {
        async fn put(
            &self,
            _agent_id: &AgentId,
            _content_type: AvatarContentType,
            _bytes: Vec<u8>,
        ) -> Result<AgentAvatarBlobPointer, AvatarStorageFailure> {
            let n = self.next.fetch_add(1, Ordering::SeqCst);
            self.puts.fetch_add(1, Ordering::SeqCst);
            Ok(ptr(&format!("agents/mint/blob-{n}")))
        }

        async fn delete(&self, blob: &OwnedAgentAvatarBlob) -> Result<(), AvatarStorageFailure> {
            self.deletes
                .lock()
                .unwrap()
                .push((blob.owner().clone(), blob.pointer().as_str().to_owned()));
            if self.fail_delete {
                #[derive(Debug, Error)]
                #[error("blob backend down")]
                struct BackendDown;
                Err(AvatarStorageFailure::from_source(BackendDown))
            } else {
                Ok(())
            }
        }
    }

    /// A fake writer returning a fixed fact (or a persistence fault), ignoring the
    /// decision: the DOMAIN replays/derives the verdict from the witnessed before-image
    /// regardless, which is exactly what the falsifiers probe.
    #[allow(clippy::large_enum_variant)]
    #[derive(Clone)]
    enum FakeWrites {
        Fault,
        Unexpected,
        Row(AgentProfileRowWrite),
    }
    impl AgentProfileWriteUnitOfWork for FakeWrites {
        async fn write_agent_row<D>(
            &self,
            _agent: &AgentId,
            _decide: D,
            _now_ms: i64,
        ) -> Result<AgentProfileRowWrite, PersistenceFailure>
        where
            D: FnOnce(&AgentRowBefore) -> Option<AdmittedAgentProfileEdit> + Send,
        {
            match self {
                FakeWrites::Unexpected => panic!("row write must not be reached"),
                FakeWrites::Fault => {
                    #[derive(Debug, Error)]
                    #[error("db down")]
                    struct DbDown;
                    Err(PersistenceFailure::from_source(DbDown))
                }
                FakeWrites::Row(row) => Ok(row.clone()),
            }
        }
    }

    #[derive(Clone)]
    struct FakeSink {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }
    impl FakeSink {
        fn new(fail: bool) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                fail,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    impl AgentChangedSink for FakeSink {
        async fn agent_changed(&self, _agent: &AgentId) -> Result<(), AgentChangedAnnounceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                #[derive(Debug, Error)]
                #[error("bus down")]
                struct BusDown;
                Err(AgentChangedAnnounceFailure::from_source(BusDown))
            } else {
                Ok(())
            }
        }
    }

    // ---- before-image / committed builders ----

    fn ctx() -> AgentTuningContext {
        AgentTuningContext {
            runtime: RuntimeKind::Claude,
            model: None,
            reasoning_effort: None,
        }
    }

    /// A before-image bound to `agent()` with the given text + avatar state.
    fn before(
        display_name: &str,
        description: Option<&str>,
        avatar: AgentAvatarRow,
    ) -> AgentRowBefore {
        AgentRowBefore {
            agent_id: agent(),
            display_name: display_name.to_owned(),
            description: description.map(str::to_owned),
            service_tier: None,
            avatar,
            context: ctx(),
        }
    }

    /// The committed row a trustworthy write returns.
    fn committed(
        display_name: &str,
        description: Option<&str>,
        avatar: AgentAvatarRow,
    ) -> CommittedAgentRow {
        CommittedAgentRow {
            id: agent(),
            display_name: display_name.to_owned(),
            description: description.map(str::to_owned),
            model: None,
            reasoning_effort: None,
            service_tier: None,
            avatar,
            updated_at: NOW,
        }
    }

    fn present(raw: &str) -> AgentAvatarRow {
        AgentAvatarRow::Present {
            pointer: ptr(raw),
            content_type: AvatarContentType::Png,
        }
    }

    fn set_avatar() -> InternalAvatarEdit {
        InternalAvatarEdit::Set {
            content_type: AvatarContentType::Png,
            bytes: vec![1, 2, 3],
            design: None,
        }
    }

    fn text_only(display_name: &str) -> InternalProfileEdit {
        InternalProfileEdit {
            display_name: RequiredEdit::Set(display_name.to_owned()),
            description: RequiredEdit::Leave,
            avatar: InternalAvatarEdit::Leave,
        }
    }

    // ================= combined text + avatar: single UoW / single announce =================

    #[tokio::test]
    async fn a_combined_text_and_avatar_set_announces_once_after_a_trustworthy_fact() {
        // before Absent → first avatar; combined with a display_name change.
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before("Old", None, AgentAvatarRow::Absent),
            row: committed("New", None, present(FRESH)),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Set("New".to_owned()),
            description: RequiredEdit::Leave,
            avatar: set_avatar(),
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        let InternalProfileWriteOutcome::Committed(effect) = outcome else {
            panic!("expected Committed");
        };
        let (id, announce, cleanup) = effect.into_parts();
        assert_eq!(
            id,
            agent(),
            "the committed id is carried for the post-commit reproject"
        );
        assert!(matches!(announce, AgentChangedAnnounce::Delivered));
        assert!(
            matches!(cleanup, BlobCleanup::NothingToClean),
            "no prior blob on a first set"
        );
        assert_eq!(
            blobs.puts(),
            1,
            "exactly one blob put for the combined edit"
        );
        assert!(blobs.deleted_pointers().is_empty());
        assert_eq!(
            sink.calls(),
            1,
            "a combined text+avatar edit announces EXACTLY once"
        );
    }

    #[tokio::test]
    async fn a_combined_text_and_avatar_replacement_cleans_the_prior_blob_only() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before("Old", None, present("agents/prior/blob-9")),
            // an empty-string description is a Set that persists as "" — the after-image
            // must reflect it, or the full-column seal would (correctly) reject the row.
            row: committed("New", Some(""), present(FRESH)),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Set("New".to_owned()),
            description: RequiredEdit::Set(String::new()), // empty description is a Set, not a clear
            avatar: set_avatar(),
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        let InternalProfileWriteOutcome::Committed(effect) = outcome else {
            panic!("expected Committed");
        };
        let (_id, _announce, cleanup) = effect.into_parts();
        assert!(matches!(cleanup, BlobCleanup::Deleted));
        assert_eq!(
            blobs.deleted(),
            vec![(agent(), "agents/prior/blob-9".to_owned())],
            "the prior delete is bound to the subject; the fresh blob is retained"
        );
        assert_eq!(sink.calls(), 1);
    }

    // ================= text-only: no avatar cleanup, one announce =================

    #[tokio::test]
    async fn a_text_only_edit_never_cleans_the_avatar_blob_even_when_present() {
        // before has a present avatar; the edit touches only display_name. The avatar
        // blob must NOT be cleaned — a text-only edit is not an avatar transition.
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before("Old", None, present("agents/keep/blob-7")),
            row: committed("New", None, present("agents/keep/blob-7")),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome =
            update_internal_profile(&writes, &blobs, &sink, &agent(), text_only("New"), NOW).await;
        let InternalProfileWriteOutcome::Committed(effect) = outcome else {
            panic!("expected Committed");
        };
        let (_id, _announce, cleanup) = effect.into_parts();
        assert!(matches!(cleanup, BlobCleanup::NothingToClean));
        assert!(
            blobs.deleted_pointers().is_empty(),
            "a text-only edit must never touch the avatar blob"
        );
        assert_eq!(blobs.puts(), 0);
        assert_eq!(sink.calls(), 1);
    }

    // ================= clear no-op vs clear-with-text =================

    #[tokio::test]
    async fn an_avatar_clear_on_an_absent_avatar_with_no_text_is_a_no_op() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::Unchanged {
            before: before("Name", None, AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Leave,
            description: RequiredEdit::Leave,
            avatar: InternalAvatarEdit::Clear,
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        assert!(
            matches!(outcome, InternalProfileWriteOutcome::NoOp { agent_id } if agent_id == agent())
        );
        assert!(blobs.deleted_pointers().is_empty());
        assert_eq!(blobs.puts(), 0);
        assert_eq!(sink.calls(), 0, "a genuine no-op announces nothing");
    }

    #[tokio::test]
    async fn an_avatar_clear_on_an_absent_avatar_with_a_text_edit_announces_once() {
        // clear-on-absent is no transition, but the text edit warrants the single commit
        // and the single announce; the avatar blob is untouched.
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before("Old", None, AgentAvatarRow::Absent),
            row: committed("New", None, AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Set("New".to_owned()),
            description: RequiredEdit::Leave,
            avatar: InternalAvatarEdit::Clear,
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        let InternalProfileWriteOutcome::Committed(effect) = outcome else {
            panic!("expected Committed");
        };
        let (_id, _announce, cleanup) = effect.into_parts();
        assert!(matches!(cleanup, BlobCleanup::NothingToClean));
        assert!(blobs.deleted_pointers().is_empty());
        assert_eq!(sink.calls(), 1);
    }

    #[tokio::test]
    async fn an_avatar_clear_on_a_present_avatar_removes_the_blob_and_announces_once() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before("Name", None, present("agents/prior/blob-9")),
            row: committed("Name", None, AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Leave,
            description: RequiredEdit::Leave,
            avatar: InternalAvatarEdit::Clear,
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        let InternalProfileWriteOutcome::Committed(effect) = outcome else {
            panic!("expected Cleared commit");
        };
        let (_id, _announce, cleanup) = effect.into_parts();
        assert!(matches!(cleanup, BlobCleanup::Deleted));
        assert_eq!(
            blobs.deleted_pointers(),
            vec!["agents/prior/blob-9".to_owned()]
        );
        assert_eq!(sink.calls(), 1);
    }

    // ================= fresh-orphan cleanup on pre-commit failures =================

    #[tokio::test]
    async fn a_no_such_agent_after_a_set_put_cleans_the_fresh_orphan_and_never_announces() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::NoSuchAgent);
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Set("New".to_owned()),
            description: RequiredEdit::Leave,
            avatar: set_avatar(),
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        let InternalProfileWriteOutcome::NoSuchAgent { orphan_cleanup } = outcome else {
            panic!("expected NoSuchAgent");
        };
        assert!(matches!(orphan_cleanup, BlobCleanup::Deleted));
        assert_eq!(blobs.deleted_pointers(), vec![FRESH.to_owned()]);
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_persistence_fault_after_a_set_put_cleans_the_fresh_orphan_and_carries_the_cause() {
        let writes = FakeWrites::Fault;
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Set("New".to_owned()),
            description: RequiredEdit::Leave,
            avatar: set_avatar(),
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        let InternalProfileWriteOutcome::WriteFaulted {
            cause,
            orphan_cleanup,
        } = outcome
        else {
            panic!("expected WriteFaulted");
        };
        assert!(matches!(cause, AgentProfileWriteError::Persistence(_)));
        assert!(matches!(orphan_cleanup, BlobCleanup::Deleted));
        assert_eq!(blobs.deleted_pointers(), vec![FRESH.to_owned()]);
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_blob_put_failure_reports_storage_failed_with_no_write_or_announce() {
        // A put failure short-circuits before any row write.
        #[derive(Clone)]
        struct FailingPut;
        impl AgentAvatarBlobRead for FailingPut {
            async fn read(
                &self,
                _blob: &OwnedAgentAvatarBlob,
            ) -> Result<AvatarBlobReadOutcome, AvatarStorageFailure> {
                Ok(AvatarBlobReadOutcome::Missing)
            }
        }
        impl AgentAvatarBlobStore for FailingPut {
            async fn put(
                &self,
                _agent_id: &AgentId,
                _content_type: AvatarContentType,
                _bytes: Vec<u8>,
            ) -> Result<AgentAvatarBlobPointer, AvatarStorageFailure> {
                #[derive(Debug, Error)]
                #[error("put down")]
                struct PutDown;
                Err(AvatarStorageFailure::from_source(PutDown))
            }
            async fn delete(
                &self,
                _blob: &OwnedAgentAvatarBlob,
            ) -> Result<(), AvatarStorageFailure> {
                Ok(())
            }
        }
        let writes = FakeWrites::Unexpected;
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Set("New".to_owned()),
            description: RequiredEdit::Leave,
            avatar: set_avatar(),
        };
        let outcome =
            update_internal_profile(&writes, &FailingPut, &sink, &agent(), edit, NOW).await;
        assert!(matches!(
            outcome,
            InternalProfileWriteOutcome::StorageFailed(_)
        ));
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_set_upload_reported_unchanged_is_untrustworthy_and_orphans_are_cleaned() {
        // A set always warrants a write; an Unchanged means a warranted write was withheld.
        let writes = FakeWrites::Row(AgentProfileRowWrite::Unchanged {
            before: before("Name", None, AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Leave,
            description: RequiredEdit::Leave,
            avatar: set_avatar(),
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        let InternalProfileWriteOutcome::WriteFaulted {
            cause,
            orphan_cleanup,
        } = outcome
        else {
            panic!("expected WriteFaulted");
        };
        assert!(matches!(
            cause,
            AgentProfileWriteError::UntrustworthyWriteFact
        ));
        assert!(matches!(orphan_cleanup, BlobCleanup::Deleted));
        assert_eq!(blobs.deleted_pointers(), vec![FRESH.to_owned()]);
        assert_eq!(sink.calls(), 0);
    }

    // ================= durable-but-untrustworthy: retain fresh =================

    #[tokio::test]
    async fn a_commit_bound_to_another_agent_retains_the_fresh_blob_and_never_announces() {
        let mut other = before("Old", None, AgentAvatarRow::Absent);
        other.agent_id = AgentId::parse("agent-other").unwrap();
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: other,
            row: committed("New", None, present(FRESH)),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Set("New".to_owned()),
            description: RequiredEdit::Leave,
            avatar: set_avatar(),
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        let InternalProfileWriteOutcome::CommitUntrustworthy { retained_fresh } = outcome else {
            panic!("expected CommitUntrustworthy");
        };
        assert_eq!(retained_fresh.as_ref().map(|p| p.as_str()), Some(FRESH));
        assert!(
            blobs.deleted_pointers().is_empty(),
            "a durable commit's fresh blob must never be deleted, even on a bad witness"
        );
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_commit_whose_tuning_column_was_tampered_retains_the_fresh_blob() {
        // The committed avatar correctly points at FRESH, but a column the internal
        // capability leaves untouched (service_tier) came back changed: the full-column seal
        // fails, yet the durable row references the fresh blob, so it is RETAINED.
        let mut tampered = committed("New", None, present(FRESH));
        tampered.service_tier = Some("priority".into());
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before("Old", None, AgentAvatarRow::Absent),
            row: tampered,
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Set("New".to_owned()),
            description: RequiredEdit::Leave,
            avatar: set_avatar(),
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        let InternalProfileWriteOutcome::CommitUntrustworthy { retained_fresh } = outcome else {
            panic!("expected CommitUntrustworthy on a tampered leave-column");
        };
        assert_eq!(retained_fresh.as_ref().map(|p| p.as_str()), Some(FRESH));
        assert!(blobs.deleted_pointers().is_empty());
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_text_only_untrustworthy_commit_retains_no_fresh_blob() {
        // A cross-subject witness on a text-only edit: still untrustworthy (safe-500), but
        // there is no fresh blob to retain.
        let mut other = before("Old", None, AgentAvatarRow::Absent);
        other.agent_id = AgentId::parse("agent-other").unwrap();
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: other,
            row: committed("New", None, AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome =
            update_internal_profile(&writes, &blobs, &sink, &agent(), text_only("New"), NOW).await;
        assert!(matches!(
            outcome,
            InternalProfileWriteOutcome::CommitUntrustworthy {
                retained_fresh: None
            }
        ));
        assert!(blobs.deleted_pointers().is_empty());
        assert_eq!(blobs.puts(), 0);
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_committed_edits_failed_prior_cleanup_is_carried_not_dropped() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before("Old", None, present("agents/prior/blob-9")),
            row: committed("New", None, present(FRESH)),
        });
        let blobs = FakeBlobs::new(true); // the prior-blob delete fails
        let sink = FakeSink::new(false);
        let edit = InternalProfileEdit {
            display_name: RequiredEdit::Set("New".to_owned()),
            description: RequiredEdit::Leave,
            avatar: set_avatar(),
        };
        let outcome = update_internal_profile(&writes, &blobs, &sink, &agent(), edit, NOW).await;
        let InternalProfileWriteOutcome::Committed(effect) = outcome else {
            panic!("expected Committed even when the prior cleanup fails");
        };
        let (_id, announce, cleanup) = effect.into_parts();
        assert!(matches!(cleanup, BlobCleanup::Failed(_)));
        assert!(matches!(announce, AgentChangedAnnounce::Delivered));
    }

    #[tokio::test]
    async fn a_failed_announce_does_not_downgrade_a_committed_write() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before("Old", None, AgentAvatarRow::Absent),
            row: committed("New", None, AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(true); // announce fails
        let outcome =
            update_internal_profile(&writes, &blobs, &sink, &agent(), text_only("New"), NOW).await;
        let InternalProfileWriteOutcome::Committed(effect) = outcome else {
            panic!("expected Committed even when the announce fails");
        };
        let (_id, announce, _cleanup) = effect.into_parts();
        assert!(matches!(announce, AgentChangedAnnounce::Failed(_)));
        assert_eq!(sink.calls(), 1);
    }
}
