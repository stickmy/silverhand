//! The agent avatar upload/clear use cases for `POST` and `DELETE
//! /api/agents/{id}/avatar` — the orchestration bounded context that drives the blob
//! store and the SINGLE canonical `agents`-row writer
//! ([`crate::identity_access::agent_profile_write::AgentProfileWriteUnitOfWork`]) to repoint (or clear)
//! the paired avatar columns, cleaning up orphaned/replaced blobs and firing the owed
//! `agent_changed` announce.
//!
//! ## This module owns the LIFECYCLE, not a second row port or edit algebra
//!
//! It declares no row port, no edit type, and no persistence fact of its own: the
//! canonical writer, the sealed [`crate::identity_access::agent_profile_write::AdmittedAgentProfileEdit`]
//! (minted only by `admit_avatar_assignment` / `admit_avatar_removal`), and the
//! full-column seal all live in `agent_profile_write`. Payload validation is
//! [`crate::identity_access::avatar::validate_avatar`] — the ONE authority for the empty→size→MIME
//! refusal order, the 5 MiB cap, and every `code`/`message`; this module never restates
//! a constant or a rejection table. The boundary lowercases the declared MIME before
//! handing it here. What IS agent-specific and new lives here alone: the candidate/
//! use-case shapes and the blob/write OUTCOMES.
//!
//! ## Put-then-write, with orphan/replacement cleanup carried to the boundary
//!
//! Upload order matches the product: validate first (a bad payload costs no blob
//! write), then put the FRESH blob, then repoint the row through the canonical writer.
//! An upload's decision ALWAYS assigns, so every non-`Committed` fact after the put —
//! `NoSuchAgent`, a persistence fault, an untrustworthy `Unchanged`, a cross-subject or
//! seal-mismatch `Committed` — orphans the just-put blob; each best-effort deletes it
//! and CARRIES the [`BlobCleanup`] fate so the boundary can log a leak (product maps
//! them to `404` / safe-`500` respectively). A trustworthy commit instead cleans the
//! PRIOR blob the row used to point at (if any) and announces exactly once.
//!
//! Clear decides from the before-image under the lock: a present avatar is removed
//! (→ `Committed`, prior blob cleaned, one announce), an already-absent avatar is a
//! legitimate NO-OP (`None` → `Unchanged`, no write/delete/announce). Because the
//! decision is state-dependent, the domain re-derives it against the witnessed
//! before-image: an `Unchanged` whose before was Present, or a `Committed` whose before
//! was Absent, is an untrustworthy fact (safe-`500`), never laundered into a success.

use crate::{
    foundation::identity::AgentId,
    identity_access::agent_avatar::{
        AgentAvatarAssignment, AgentAvatarBlobPointer, AgentAvatarBlobStore, AgentAvatarRow,
        OwnedAgentAvatarBlob,
    },
    identity_access::agent_profile_write::{
        AgentChangedAnnounce, AgentChangedSink, AgentProfileRowWrite, AgentProfileWriteError,
        AgentProfileWriteUnitOfWork, admit_avatar_assignment, admit_avatar_removal, seal_agent_row,
    },
    identity_access::avatar::{
        AvatarRejection, AvatarStorageFailure, BlobCleanup, CandidateAvatar, validate_avatar,
    },
};

// ===========================================================================
// The committed effect: an id for the post-commit DTO re-read, plus the two
// carried residues (announce fate + blob cleanup fate).
// ===========================================================================

/// A committed avatar write (upload OR clear) together with the fate of its
/// post-commit `agent_changed` announce AND its best-effort blob cleanup (the replaced
/// blob on an upload, the removed blob on a clear). Reachable only through
/// [`into_parts`](Self::into_parts), so a boundary taking the id for the wire re-read
/// receives BOTH residues in the same move and cannot drop them. It carries only the
/// id: the wire DTO is composed by re-reading through the canonical `agent_detail`
/// projector, never from this effect.
#[derive(Debug)]
#[must_use = "a committed agent avatar write carries an announce fate and a blob-cleanup fate that must be handled"]
pub struct AnnouncedAgentAvatarWrite {
    agent_id: AgentId,
    announce: AgentChangedAnnounce,
    blob_cleanup: BlobCleanup,
}

impl AnnouncedAgentAvatarWrite {
    pub fn into_parts(self) -> (AgentId, AgentChangedAnnounce, BlobCleanup) {
        (self.agent_id, self.announce, self.blob_cleanup)
    }
}

// ===========================================================================
// Upload.
// ===========================================================================

/// The closed outcome of [`upload_agent_avatar`]. A failure reached BEFORE the durable
/// commit (`NoSuchAgent`, `Unchanged`, a pre-commit persistence fault) leaves the fresh
/// blob provably unreferenced, so it is cleaned and its [`BlobCleanup`] fate carried. A
/// failure on a DURABLE commit ([`CommitUntrustworthy`](Self::CommitUntrustworthy))
/// cannot prove the fresh blob unreferenced — the always-assign UPDATE persisted — so
/// the blob is deliberately RETAINED, never cleaned.
#[derive(Debug)]
#[must_use = "an avatar upload outcome carries a committed effect, a blob-cleanup fate, or a retained-blob residue that must be handled"]
pub enum AgentAvatarUploadOutcome {
    /// The row was repointed at the fresh blob and the change announced. Carries the
    /// replaced prior blob's cleanup fate. → product `200` (DTO re-read).
    Committed(AnnouncedAgentAvatarWrite),
    /// The row vanished between the visibility gate and the repoint. No durable write
    /// committed, so the fresh blob is orphaned and cleaned best-effort. → product `404`.
    NoSuchAgent { orphan_cleanup: BlobCleanup },
    /// The payload was refused before any blob was put — the empty/too-large/
    /// unsupported-type `400`/`413`/`415`. No blob or row was touched.
    Rejected(AvatarRejection),
    /// The blob `put` itself failed; no row was written and no blob persisted. → the
    /// product's `avatar_storage_failed` `400`.
    StorageFailed(AvatarStorageFailure),
    /// A pre-commit write fault (a persistence error, or an always-assign upload the
    /// adapter reported `Unchanged`): no durable commit, so the fresh blob is a true
    /// orphan, cleaned best-effort with its fate carried. → safe-`500`.
    WriteFaulted {
        cause: AgentProfileWriteError,
        orphan_cleanup: BlobCleanup,
    },
    /// The row committed durably — the always-assign UPDATE persisted, so the row now
    /// references the FRESH blob — yet the returned witness could not be trusted (a
    /// cross-subject before-image, or a full-column seal mismatch, e.g. a tampered
    /// non-avatar column). The fresh blob is DELIBERATELY retained, never deleted:
    /// deleting it would strand the committed row on a dangling pointer. Custody of the
    /// committed blob is indeterminate; `retained_fresh` is recorded for reconciliation.
    /// This is NOT `BlobCleanup::NothingToClean` — a residue exists and cleanup was
    /// intentionally not attempted. → safe-`500`, no announce.
    CommitUntrustworthy {
        retained_fresh: AgentAvatarBlobPointer,
    },
}

/// Upload (or replace) an agent's avatar. `subject` MUST have already cleared the
/// route's existence + visibility + authority gates; this use case owns only the
/// validate → put → repoint → cleanup → announce lifecycle over the canonical writer.
pub async fn upload_agent_avatar<W, B, S>(
    writes: &W,
    blobs: &B,
    announce_sink: &S,
    subject: &AgentId,
    candidate: CandidateAvatar,
    now_ms: i64,
) -> AgentAvatarUploadOutcome
where
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
{
    let CandidateAvatar {
        bytes,
        declared_content_type,
        design,
    } = candidate;

    // Validate BEFORE any blob write, reusing the single avatar validator: no second
    // table of codes/messages or size cap lives in this module.
    let content_type = match validate_avatar(bytes.len() as u64, &declared_content_type) {
        Ok(content_type) => content_type,
        Err(rejection) => return AgentAvatarUploadOutcome::Rejected(rejection),
    };

    // Put the fresh blob. A failure here wrote no row, so there is no orphan yet.
    let pointer = match blobs.put(subject, content_type, bytes).await {
        Ok(pointer) => pointer,
        Err(failure) => return AgentAvatarUploadOutcome::StorageFailed(failure),
    };
    let assignment = AgentAvatarAssignment {
        pointer: pointer.clone(),
        content_type,
        design,
    };

    // The decision ALWAYS assigns — an upload is never a legality refusal — so any fact
    // other than a trustworthy commit orphans the fresh blob.
    let decision = assignment.clone();
    let write = writes
        .write_agent_row(
            subject,
            move |_before| Some(admit_avatar_assignment(&decision)),
            now_ms,
        )
        .await;
    let write = match write {
        Ok(write) => write,
        Err(failure) => {
            // Persistence faulted after the put: clean the fresh orphan, carry its fate.
            let orphan_cleanup = cleanup_agent_blob(blobs, subject, &pointer).await;
            return AgentAvatarUploadOutcome::WriteFaulted {
                cause: AgentProfileWriteError::Persistence(failure),
                orphan_cleanup,
            };
        }
    };

    match write {
        AgentProfileRowWrite::NoSuchAgent => {
            let orphan_cleanup = cleanup_agent_blob(blobs, subject, &pointer).await;
            AgentAvatarUploadOutcome::NoSuchAgent { orphan_cleanup }
        }
        AgentProfileRowWrite::Unchanged { before: _ } => {
            // An upload's decision returns Some, so a write that changed nothing means a
            // warranted write was withheld — an untrustworthy fact. Clean the orphan.
            let orphan_cleanup = cleanup_agent_blob(blobs, subject, &pointer).await;
            AgentAvatarUploadOutcome::WriteFaulted {
                cause: AgentProfileWriteError::UntrustworthyWriteFact,
                orphan_cleanup,
            }
        }
        AgentProfileRowWrite::Committed { before, row } => {
            // Bind the witness to the subject and seal the FULL after-image against the
            // replayed assignment before trusting the commit. Any failure orphans the
            // fresh blob (safe-500 with the fate carried).
            let admitted = admit_avatar_assignment(&assignment);
            if before.agent_id != *subject
                || seal_agent_row(subject, &before, &admitted, &row, now_ms).is_err()
            {
                // The always-assign UPDATE committed durably, so the persisted row
                // references the FRESH blob regardless of WHY the witness is untrusted
                // (a truthful commit can't have withheld our SET; an untruthful one can't
                // be trusted to have). Deleting the fresh blob now would strand the row on
                // a dangling pointer, so it is RETAINED and its custody recorded as
                // indeterminate — never cleaned, never laundered to NothingToClean.
                return AgentAvatarUploadOutcome::CommitUntrustworthy {
                    retained_fresh: pointer,
                };
            }
            // The commit is trustworthy. The prior blob the row pointed at (if any) is now
            // unreferenced — clean it best-effort and carry its fate. The fresh blob is
            // the committed pointer and is kept.
            let blob_cleanup = match before.avatar {
                AgentAvatarRow::Present { pointer: prior, .. } => {
                    cleanup_agent_blob(blobs, subject, &prior).await
                }
                AgentAvatarRow::Absent => BlobCleanup::NothingToClean,
            };
            let announce = announce_agent_changed(announce_sink, subject).await;
            AgentAvatarUploadOutcome::Committed(AnnouncedAgentAvatarWrite {
                agent_id: row.id,
                announce,
                blob_cleanup,
            })
        }
    }
}

// ===========================================================================
// Clear.
// ===========================================================================

/// The closed outcome of [`clear_agent_avatar`]. The clear path puts no blob, so its
/// safe-`500`s ride the `Result::Err` rather than a carried orphan.
#[derive(Debug)]
#[must_use = "an avatar clear outcome carries a committed effect that must be handled"]
pub enum AgentAvatarClearOutcome {
    /// A present avatar was removed and the change announced. Carries the removed
    /// blob's cleanup fate. → product `200` (DTO re-read).
    Cleared(AnnouncedAgentAvatarWrite),
    /// The avatar was already absent: a legitimate no-op. No write, no delete, no
    /// announce. → product `200` (current DTO re-read). Carries the source-bound id.
    NoOp { agent_id: AgentId },
    /// No such agent row. → product `404`.
    NoSuchAgent,
}

/// Clear an agent's avatar. `subject` MUST have already cleared the route's existence +
/// visibility + authority gates. The decision is read under the write lock from the
/// before-image: a present avatar is removed, an absent one is left untouched.
pub async fn clear_agent_avatar<W, B, S>(
    writes: &W,
    blobs: &B,
    announce_sink: &S,
    subject: &AgentId,
    now_ms: i64,
) -> Result<AgentAvatarClearOutcome, AgentProfileWriteError>
where
    W: AgentProfileWriteUnitOfWork,
    B: AgentAvatarBlobStore,
    S: AgentChangedSink,
{
    // Remove only when the before-image shows a present blob; an already-absent avatar
    // is a no-op (None → Unchanged), never a write.
    let write = writes
        .write_agent_row(
            subject,
            |before| match &before.avatar {
                AgentAvatarRow::Present { .. } => Some(admit_avatar_removal()),
                AgentAvatarRow::Absent => None,
            },
            now_ms,
        )
        .await?;

    match write {
        AgentProfileRowWrite::NoSuchAgent => Ok(AgentAvatarClearOutcome::NoSuchAgent),
        AgentProfileRowWrite::Unchanged { before } => {
            // Bind the witness before trusting its state. A genuine no-op is an absent
            // before; a present before means the warranted removal was silently withheld.
            if before.agent_id != *subject {
                return Err(AgentProfileWriteError::UntrustworthyWriteFact);
            }
            match &before.avatar {
                AgentAvatarRow::Absent => Ok(AgentAvatarClearOutcome::NoOp {
                    agent_id: before.agent_id,
                }),
                AgentAvatarRow::Present { .. } => {
                    Err(AgentProfileWriteError::UntrustworthyWriteFact)
                }
            }
        }
        AgentProfileRowWrite::Committed { before, row } => {
            if before.agent_id != *subject {
                return Err(AgentProfileWriteError::UntrustworthyWriteFact);
            }
            // A committed clear must have removed a present blob; an absent before means
            // nothing warranted the write.
            let prior = match &before.avatar {
                AgentAvatarRow::Present { pointer, .. } => pointer.clone(),
                AgentAvatarRow::Absent => {
                    return Err(AgentProfileWriteError::UntrustworthyWriteFact);
                }
            };
            let admitted = admit_avatar_removal();
            seal_agent_row(subject, &before, &admitted, &row, now_ms)?;
            let blob_cleanup = cleanup_agent_blob(blobs, subject, &prior).await;
            let announce = announce_agent_changed(announce_sink, subject).await;
            Ok(AgentAvatarClearOutcome::Cleared(
                AnnouncedAgentAvatarWrite {
                    agent_id: row.id,
                    announce,
                    blob_cleanup,
                },
            ))
        }
    }
}

// ===========================================================================
// Shared effect plumbing.
// ===========================================================================

/// Run the best-effort post-commit blob delete and fold its result into a
/// [`BlobCleanup`] (a failure is carried, never propagated — the DB pointer is already
/// authoritative). Mirrors the user path's `cleanup_blob`, over the agent custody. The
/// `owner` is the agent whose lifecycle warrants the delete (the write subject); binding
/// it means a prior/fresh pointer that has been cross-wired to ANOTHER agent's key is
/// refused by the adapter rather than deleting that agent's blob — the refusal rides the
/// carried `BlobCleanup::Failed` fate for the boundary to log.
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
        identity_access::agent_avatar::{AgentAvatarBlobRead, OwnedAgentAvatarBlob},
        identity_access::agent_profile_write::{
            AdmittedAgentProfileEdit, AgentChangedAnnounceFailure, AgentRowBefore,
            AgentTuningContext, CommittedAgentRow,
        },
        identity_access::agents::runtime::RuntimeKind,
        identity_access::avatar::{AvatarBlobReadOutcome, AvatarContentType},
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use thiserror::Error;

    const NOW: i64 = 9_000;
    /// The pointer the fake blob store mints on its FIRST `put` — deterministic so the
    /// committed row can name it.
    const FRESH: &str = "agents/mint/blob-0";

    fn agent() -> AgentId {
        AgentId::parse("agent-1").unwrap()
    }

    fn ptr(raw: &str) -> AgentAvatarBlobPointer {
        AgentAvatarBlobPointer::parse(raw).unwrap()
    }

    // ---- fakes ----

    /// A fake blob store: `put` mints a deterministic sequential pointer and counts
    /// puts; `delete` records the deleted pointer and can be made to fail. `read` is
    /// unused by the write use cases and returns a clean `Missing`.
    #[derive(Clone)]
    struct FakeBlobs {
        puts: Arc<AtomicUsize>,
        next: Arc<AtomicUsize>,
        deletes: Arc<Mutex<Vec<String>>>,
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
        fn deleted(&self) -> Vec<String> {
            self.deletes.lock().unwrap().clone()
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
                .push(blob.pointer().as_str().to_owned());
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
    // A test double holding one owned fact per case; the size gap between the fault and
    // the fact carries no runtime cost worth an extra box here.
    #[allow(clippy::large_enum_variant)]
    #[derive(Clone)]
    enum FakeWrites {
        Fault,
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

    /// A before-image bound to `agent()` with the given avatar state; tuning columns are
    /// arbitrary but must match the committed row (the seal proves they were untouched).
    fn before(avatar: AgentAvatarRow) -> AgentRowBefore {
        AgentRowBefore {
            agent_id: agent(),
            display_name: "Agent One".to_owned(),
            description: None,
            service_tier: None,
            avatar,
            context: ctx(),
        }
    }

    /// The committed row a trustworthy write returns: tuning columns identical to the
    /// before-image (untouched), the avatar column set to `avatar`, `updated_at` = NOW.
    fn committed(avatar: AgentAvatarRow) -> CommittedAgentRow {
        CommittedAgentRow {
            id: agent(),
            display_name: "Agent One".to_owned(),
            description: None,
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

    fn candidate() -> CandidateAvatar {
        CandidateAvatar {
            bytes: vec![1, 2, 3],
            declared_content_type: "image/png".to_owned(),
            design: None,
        }
    }

    // ================= upload =================

    #[tokio::test]
    async fn an_empty_payload_is_rejected_before_any_blob_is_put() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::NoSuchAgent);
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let empty = CandidateAvatar {
            bytes: vec![],
            declared_content_type: "image/png".to_owned(),
            design: None,
        };
        let outcome = upload_agent_avatar(&writes, &blobs, &sink, &agent(), empty, NOW).await;
        assert!(matches!(
            outcome,
            AgentAvatarUploadOutcome::Rejected(AvatarRejection::Empty)
        ));
        assert_eq!(blobs.puts(), 0, "a refused payload must not put a blob");
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_first_upload_commits_cleans_nothing_prior_and_announces_once() {
        // before Absent → the committed row carries the fresh pointer; no prior blob.
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before(AgentAvatarRow::Absent),
            row: committed(present(FRESH)),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = upload_agent_avatar(&writes, &blobs, &sink, &agent(), candidate(), NOW).await;
        let AgentAvatarUploadOutcome::Committed(effect) = outcome else {
            panic!("expected Committed");
        };
        let (id, announce, cleanup) = effect.into_parts();
        assert_eq!(id, agent());
        assert!(matches!(announce, AgentChangedAnnounce::Delivered));
        assert!(matches!(cleanup, BlobCleanup::NothingToClean));
        assert_eq!(blobs.puts(), 1);
        assert!(blobs.deleted().is_empty(), "no prior blob to clean");
        assert_eq!(sink.calls(), 1);
    }

    #[tokio::test]
    async fn a_replacement_upload_cleans_the_prior_blob_and_keeps_the_fresh_one() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before(present("agents/prior/blob-9")),
            row: committed(present(FRESH)),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = upload_agent_avatar(&writes, &blobs, &sink, &agent(), candidate(), NOW).await;
        let AgentAvatarUploadOutcome::Committed(effect) = outcome else {
            panic!("expected Committed");
        };
        let (_id, _announce, cleanup) = effect.into_parts();
        assert!(matches!(cleanup, BlobCleanup::Deleted));
        assert_eq!(
            blobs.deleted(),
            vec!["agents/prior/blob-9".to_owned()],
            "the PRIOR blob is cleaned, never the fresh committed one"
        );
    }

    #[tokio::test]
    async fn a_no_such_agent_after_put_cleans_the_fresh_orphan_and_never_announces() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::NoSuchAgent);
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = upload_agent_avatar(&writes, &blobs, &sink, &agent(), candidate(), NOW).await;
        let AgentAvatarUploadOutcome::NoSuchAgent { orphan_cleanup } = outcome else {
            panic!("expected NoSuchAgent");
        };
        assert!(matches!(orphan_cleanup, BlobCleanup::Deleted));
        assert_eq!(blobs.deleted(), vec![FRESH.to_owned()]);
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_persistence_fault_after_put_cleans_the_fresh_orphan_and_carries_the_cause() {
        let writes = FakeWrites::Fault;
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = upload_agent_avatar(&writes, &blobs, &sink, &agent(), candidate(), NOW).await;
        let AgentAvatarUploadOutcome::WriteFaulted {
            cause,
            orphan_cleanup,
        } = outcome
        else {
            panic!("expected WriteFaulted");
        };
        assert!(matches!(cause, AgentProfileWriteError::Persistence(_)));
        assert!(matches!(orphan_cleanup, BlobCleanup::Deleted));
        assert_eq!(blobs.deleted(), vec![FRESH.to_owned()]);
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn an_upload_that_changed_nothing_is_untrustworthy_and_orphans_are_cleaned() {
        // A liar adapter reports Unchanged for an upload whose decision always assigns.
        let writes = FakeWrites::Row(AgentProfileRowWrite::Unchanged {
            before: before(AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = upload_agent_avatar(&writes, &blobs, &sink, &agent(), candidate(), NOW).await;
        let AgentAvatarUploadOutcome::WriteFaulted {
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
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_commit_bound_to_another_agent_retains_the_fresh_blob_and_never_announces() {
        // A durable commit whose before-image is bound to a different agent: the fact is
        // untrustworthy, but the UPDATE committed, so the fresh blob may be referenced by
        // the row. It must be RETAINED (never deleted), and never announced.
        let mut other = before(AgentAvatarRow::Absent);
        other.agent_id = AgentId::parse("agent-other").unwrap();
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: other,
            row: committed(present(FRESH)), // row.id == subject, would otherwise seal cleanly
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = upload_agent_avatar(&writes, &blobs, &sink, &agent(), candidate(), NOW).await;
        let AgentAvatarUploadOutcome::CommitUntrustworthy { retained_fresh } = outcome else {
            panic!("expected CommitUntrustworthy");
        };
        assert_eq!(retained_fresh.as_str(), FRESH);
        assert!(
            blobs.deleted().is_empty(),
            "a durable commit's fresh blob must never be deleted, even on a bad witness"
        );
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_commit_whose_committed_avatar_disagrees_retains_the_fresh_blob() {
        // The row came back Absent though the upload set a pointer: the seal fails.
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before(AgentAvatarRow::Absent),
            row: committed(AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = upload_agent_avatar(&writes, &blobs, &sink, &agent(), candidate(), NOW).await;
        assert!(matches!(
            outcome,
            AgentAvatarUploadOutcome::CommitUntrustworthy { .. }
        ));
        assert!(blobs.deleted().is_empty());
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_commit_whose_avatar_is_fresh_but_a_tuning_column_was_tampered_retains_the_fresh_blob()
     {
        // falsifier: the committed avatar correctly points at FRESH, but a
        // NON-avatar column the avatar-only admit left untouched came back changed
        // (display_name tampered). The full-column seal fails, yet the durable row DOES
        // reference the fresh blob — so it must be RETAINED, safe-500, announce=0,
        // fresh delete=0. Deleting fresh here would strand the row on a dangling pointer.
        let mut tampered = committed(present(FRESH));
        tampered.display_name = "Tampered Name".to_owned();
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before(AgentAvatarRow::Absent),
            row: tampered,
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = upload_agent_avatar(&writes, &blobs, &sink, &agent(), candidate(), NOW).await;
        let AgentAvatarUploadOutcome::CommitUntrustworthy { retained_fresh } = outcome else {
            panic!("expected CommitUntrustworthy on a tampered non-avatar column");
        };
        assert_eq!(retained_fresh.as_str(), FRESH);
        assert!(
            blobs.deleted().is_empty(),
            "the fresh blob the durable row references must never be deleted"
        );
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_committed_uploads_failed_prior_cleanup_is_carried_not_dropped() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before(present("agents/prior/blob-9")),
            row: committed(present(FRESH)),
        });
        let blobs = FakeBlobs::new(true); // the prior-blob delete fails
        let sink = FakeSink::new(false);
        let outcome = upload_agent_avatar(&writes, &blobs, &sink, &agent(), candidate(), NOW).await;
        let AgentAvatarUploadOutcome::Committed(effect) = outcome else {
            panic!("expected Committed even when the prior cleanup fails");
        };
        let (_id, _announce, cleanup) = effect.into_parts();
        assert!(matches!(cleanup, BlobCleanup::Failed(_)));
    }

    // ================= clear =================

    #[tokio::test]
    async fn clearing_a_present_avatar_removes_the_blob_and_announces_once() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before(present("agents/prior/blob-9")),
            row: committed(AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = clear_agent_avatar(&writes, &blobs, &sink, &agent(), NOW)
            .await
            .unwrap();
        let AgentAvatarClearOutcome::Cleared(effect) = outcome else {
            panic!("expected Cleared");
        };
        let (id, announce, cleanup) = effect.into_parts();
        assert_eq!(id, agent());
        assert!(matches!(announce, AgentChangedAnnounce::Delivered));
        assert!(matches!(cleanup, BlobCleanup::Deleted));
        assert_eq!(blobs.deleted(), vec!["agents/prior/blob-9".to_owned()]);
        assert_eq!(sink.calls(), 1);
    }

    #[tokio::test]
    async fn clearing_an_absent_avatar_is_a_no_op_with_no_delete_or_announce() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::Unchanged {
            before: before(AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = clear_agent_avatar(&writes, &blobs, &sink, &agent(), NOW)
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            AgentAvatarClearOutcome::NoOp { agent_id } if agent_id == agent()
        ));
        assert!(blobs.deleted().is_empty());
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn an_unchanged_clear_whose_before_was_present_is_untrustworthy() {
        // A warranted removal was silently withheld — never launder it into a no-op.
        let writes = FakeWrites::Row(AgentProfileRowWrite::Unchanged {
            before: before(present("agents/prior/blob-9")),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let result = clear_agent_avatar(&writes, &blobs, &sink, &agent(), NOW).await;
        assert!(matches!(
            result,
            Err(AgentProfileWriteError::UntrustworthyWriteFact)
        ));
        assert!(blobs.deleted().is_empty());
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_committed_clear_whose_before_was_absent_is_untrustworthy() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: before(AgentAvatarRow::Absent),
            row: committed(AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let result = clear_agent_avatar(&writes, &blobs, &sink, &agent(), NOW).await;
        assert!(matches!(
            result,
            Err(AgentProfileWriteError::UntrustworthyWriteFact)
        ));
        assert!(blobs.deleted().is_empty());
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_clear_bound_to_another_agent_is_untrustworthy() {
        let mut other = before(present("agents/prior/blob-9"));
        other.agent_id = AgentId::parse("agent-other").unwrap();
        let writes = FakeWrites::Row(AgentProfileRowWrite::Committed {
            before: other,
            row: committed(AgentAvatarRow::Absent),
        });
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let result = clear_agent_avatar(&writes, &blobs, &sink, &agent(), NOW).await;
        assert!(matches!(
            result,
            Err(AgentProfileWriteError::UntrustworthyWriteFact)
        ));
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_missing_row_clear_is_no_such_agent() {
        let writes = FakeWrites::Row(AgentProfileRowWrite::NoSuchAgent);
        let blobs = FakeBlobs::new(false);
        let sink = FakeSink::new(false);
        let outcome = clear_agent_avatar(&writes, &blobs, &sink, &agent(), NOW)
            .await
            .unwrap();
        assert!(matches!(outcome, AgentAvatarClearOutcome::NoSuchAgent));
        assert_eq!(sink.calls(), 0);
    }
}
