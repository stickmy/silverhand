//! Which runtimes the product OFFERS on a machine, and what one detection pass
//! found for each of them.
//!
//! ## Two authorities, deliberately not one
//!
//! [`RuntimeKind`] owns what a runtime IS: the closed set every stored value must
//! parse into, every agent may report, and every wire face renders. This module
//! owns a PRODUCT decision layered on top of that set — which runtimes a person may
//! pick on a machine, and the order the catalogue lists them in.
//!
//! The two answers move for different reasons. A runtime becomes part of the
//! vocabulary the moment a row can hold it (a decoder, a teardown fact, a host
//! resource face all need it); it becomes OFFERED only when the product decides to
//! ship a picker entry for it. Folding the second into the first would mean adding a
//! variant silently offers it, and that a product-order change edits the type every
//! stored runtime is read through.
//!
//! ## The catalogue's set and order are not the caller's to choose
//!
//! A [`RuntimeCatalog`] can only be built by [`RuntimeCatalog::from_detections`],
//! which takes exactly [`SELECTABLE_RUNTIME_COUNT`] detections and pairs them,
//! positionally, with [`SELECTABLE_RUNTIMES`]. There is no constructor that accepts
//! a set of runtimes, so no adapter and no route can answer with a different
//! subset, a different order, or a duplicate — a shorter or longer answer is a
//! compile error at the adapter rather than wire drift.
//!
//! ## The four states are the detection matrix, not wire tokens
//!
//! [`RuntimeDetection`] carries the four outcomes a local probe can reach and the
//! evidence each one has. Which `state` / `reason` strings those project to is the
//! wire boundary's answer, not this module's — the projection lives beside the DTO
//! so a new arm here is a compile error there.

use std::future::Future;

use crate::identity_access::agents::runtime::RuntimeKind;

/// The runtimes the product offers for selection, in the order it offers them.
///
/// Pinned to `packages/shared/src/runtime-catalog.ts`: the entries marked
/// `selectable: true`, kept in `RUNTIME_ORDER`. `Cursor` and `Gemini` are part of
/// the vocabulary — an agent may be filed under either — but the product does not
/// offer them, so a detection pass never probes for them.
///
/// That pin is enforced, not commented: `tests/runtime_catalog_parity.rs` holds this
/// constant to a checked-in artifact the TypeScript half signs from the real product
/// catalogue, so offering a runtime — or reordering the picker — on one side only fails
/// on both.
pub const SELECTABLE_RUNTIMES: [RuntimeKind; 7] = [
    RuntimeKind::Claude,
    RuntimeKind::Codex,
    RuntimeKind::Qoder,
    RuntimeKind::Opencode,
    RuntimeKind::Pi,
    RuntimeKind::Grok,
    RuntimeKind::Antigravity,
];

/// How many runtimes a detection pass must answer for. Named so an adapter states
/// the arity by reading the owner instead of hard-coding a count that could fall
/// behind it.
pub const SELECTABLE_RUNTIME_COUNT: usize = SELECTABLE_RUNTIMES.len();

/// What one local probe concluded about one runtime.
///
/// The four arms are the four outcomes the detector can reach, and each carries
/// exactly the evidence that outcome has: a resolved binary with a version, a
/// resolved binary whose version could not be read, nothing on the machine, or a
/// pass that failed before it could conclude anything. Combinations the product
/// never produces — a version without a binary, "installed" with nothing resolved —
/// are unconstructible rather than merely untested.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeDetection {
    /// An executable was found and answered `--version`.
    Resolved { binary: String, version: String },
    /// An executable was found but produced no readable version. Still installed:
    /// the binary is the evidence, and the product treats it as usable.
    VersionUnavailable { binary: String },
    /// No executable for this runtime on any searched path.
    Absent,
    /// The pass itself failed. Not "absent": absence is a conclusion, this is the
    /// lack of one, and the product keeps them apart so a broken probe is not read
    /// as a machine without the runtime.
    ProbeFailed,
}

impl RuntimeDetection {
    /// Whether the product considers the runtime present. True exactly when a binary
    /// was resolved — derived from the arm rather than stored, so it cannot disagree
    /// with the evidence.
    pub fn installed(&self) -> bool {
        self.binary().is_some()
    }

    pub fn binary(&self) -> Option<&str> {
        match self {
            RuntimeDetection::Resolved { binary, .. }
            | RuntimeDetection::VersionUnavailable { binary } => Some(binary),
            RuntimeDetection::Absent | RuntimeDetection::ProbeFailed => None,
        }
    }

    pub fn version(&self) -> Option<&str> {
        match self {
            RuntimeDetection::Resolved { version, .. } => Some(version),
            RuntimeDetection::VersionUnavailable { .. }
            | RuntimeDetection::Absent
            | RuntimeDetection::ProbeFailed => None,
        }
    }
}

/// One offered runtime and what the pass found for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetectedRuntime {
    pub kind: RuntimeKind,
    pub detection: RuntimeDetection,
}

/// One detection pass over every offered runtime.
///
/// The entries are the offered catalogue, in the offered order, once each — a
/// property of the only constructor rather than of its callers.
///
/// A pass that answers for a different number of runtimes does not compile:
///
/// ```compile_fail
/// use silverhand_backend_core::identity_access::agents::runtime_catalog::{RuntimeCatalog, RuntimeDetection};
/// // An incomplete detection pass — there is no arm of the type that
/// // could say which one was skipped, so this must not build a catalogue.
/// let _ = RuntimeCatalog::from_detections([
///     RuntimeDetection::Absent,
///     RuntimeDetection::Absent,
///     RuntimeDetection::Absent,
/// ]);
/// ```
///
/// The control for that gate — the same path, named the same way, with the arity the
/// owner states. A `compile_fail` block passes on ANY error, so without this the
/// gate above would also pass on a renamed type or a bad import.
///
/// ```
/// use silverhand_backend_core::identity_access::agents::runtime_catalog::{
///     RuntimeCatalog, RuntimeDetection, SELECTABLE_RUNTIMES,
/// };
/// let catalog = RuntimeCatalog::from_detections(
///     SELECTABLE_RUNTIMES.map(|_| RuntimeDetection::Absent),
/// );
/// let listed: Vec<_> = catalog.entries().iter().map(|entry| entry.kind).collect();
/// assert_eq!(listed, SELECTABLE_RUNTIMES.to_vec());
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeCatalog {
    entries: [DetectedRuntime; SELECTABLE_RUNTIME_COUNT],
}

impl RuntimeCatalog {
    /// Pair each detection with the runtime at the same position in
    /// [`SELECTABLE_RUNTIMES`].
    ///
    /// Positional, because that is what a caller can hold: the intended use is
    /// `SELECTABLE_RUNTIMES.map(probe)` awaited in place, where the index carries the
    /// pairing and the caller never names a runtime at all.
    pub fn from_detections(detections: [RuntimeDetection; SELECTABLE_RUNTIME_COUNT]) -> Self {
        let mut index = 0;
        Self {
            entries: detections.map(|detection| {
                let kind = SELECTABLE_RUNTIMES[index];
                index += 1;
                DetectedRuntime { kind, detection }
            }),
        }
    }

    pub fn entries(&self) -> &[DetectedRuntime] {
        &self.entries
    }
}

/// Runs one detection pass over the machine this process is on.
///
/// Infallible by contract: every way a pass can go wrong already has an arm
/// ([`RuntimeDetection::ProbeFailed`]), so there is no failure left for the caller
/// to map, and no way for a broken probe to take down the surface that reports it.
pub trait RuntimeCatalogDetector {
    fn detect_runtimes(&self) -> impl Future<Output = RuntimeCatalog> + Send;
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// The offered catalogue is a SUBSET of the vocabulary — never its own set of
    /// runtimes — and lists each one once.
    #[test]
    fn the_offered_catalogue_is_a_duplicate_free_subset_of_the_closed_runtime_set() {
        let closed: HashSet<RuntimeKind> = RuntimeKind::ALL.into_iter().collect();
        let mut seen = HashSet::new();
        for offered in SELECTABLE_RUNTIMES {
            assert!(closed.contains(&offered), "{offered:?} is not a runtime");
            assert!(seen.insert(offered), "{offered:?} is offered twice");
        }
        assert_eq!(seen.len(), SELECTABLE_RUNTIME_COUNT);
        assert!(SELECTABLE_RUNTIME_COUNT < RuntimeKind::ALL.len());
    }

    // The order is the product's, and the remainder is what the product does not
    // offer — both compared against the shared checked-in artifact by
    // `tests/runtime_catalog_parity.rs`, whose TypeScript half signs it from the real
    // product catalogue. Restating those names here would only assert this file
    // against itself, so they are deliberately absent.

    /// The pairing is the constructor's guarantee: detection `i` describes the
    /// runtime the owner lists at `i`, so a pass cannot attribute one runtime's
    /// evidence to another.
    #[test]
    fn a_pass_pairs_each_detection_with_the_runtime_at_the_same_position() {
        let catalog = RuntimeCatalog::from_detections([
            RuntimeDetection::Resolved {
                binary: "/first".to_string(),
                version: "1.0".to_string(),
            },
            RuntimeDetection::VersionUnavailable {
                binary: "/second".to_string(),
            },
            RuntimeDetection::Absent,
            RuntimeDetection::ProbeFailed,
            RuntimeDetection::Absent,
            RuntimeDetection::Absent,
            RuntimeDetection::Absent,
        ]);
        let paired: Vec<(RuntimeKind, Option<&str>)> = catalog
            .entries()
            .iter()
            .map(|entry| (entry.kind, entry.detection.binary()))
            .collect();
        assert_eq!(
            paired,
            vec![
                (RuntimeKind::Claude, Some("/first")),
                (RuntimeKind::Codex, Some("/second")),
                (RuntimeKind::Qoder, None),
                (RuntimeKind::Opencode, None),
                (RuntimeKind::Pi, None),
                (RuntimeKind::Grok, None),
                (RuntimeKind::Antigravity, None),
            ]
        );
    }

    /// `installed` follows the resolved binary, and a version is only ever reported
    /// alongside one.
    #[test]
    fn installedness_and_version_follow_the_evidence_the_arm_carries() {
        let resolved = RuntimeDetection::Resolved {
            binary: "/bin/claude".to_string(),
            version: "2.1.3".to_string(),
        };
        assert!(resolved.installed());
        assert_eq!(resolved.binary(), Some("/bin/claude"));
        assert_eq!(resolved.version(), Some("2.1.3"));

        let unverified = RuntimeDetection::VersionUnavailable {
            binary: "/bin/codex".to_string(),
        };
        assert!(unverified.installed());
        assert_eq!(unverified.binary(), Some("/bin/codex"));
        assert_eq!(unverified.version(), None);

        for concluded_nothing in [RuntimeDetection::Absent, RuntimeDetection::ProbeFailed] {
            assert!(!concluded_nothing.installed());
            assert_eq!(concluded_nothing.binary(), None);
            assert_eq!(concluded_nothing.version(), None);
        }
    }
}
