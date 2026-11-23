//! One detection pass, cached for a window, owned for the life of the process.
//!
//! ## Why the cache is part of the component and not of a caller
//!
//! A pass stats dozens of directories, may start a login shell, and spawns one child
//! per offered runtime — one of which can take twenty seconds to answer. A surface
//! that asked the machine on every request would make a cheap read expensive and
//! would let a burst multiply that cost. So the window belongs to the thing that
//! knows what a pass costs: this component holds it, and every caller that shares
//! the component shares the one window.
//!
//! Cloning shares the cache: the state is behind one [`Arc`], so a factory handed a
//! clone cannot acquire a private window. Constructing a SECOND detector would, which
//! is why exactly one is built where the process is assembled and injected from
//! there.
//!
//! ## One pass at a time
//!
//! The window is guarded by a lock the pass itself holds. A request arriving while a
//! pass runs waits for it and then reads its result, so concurrent first requests
//! produce ONE walk of the machine rather than one each — the product's cache admits
//! that fan-out, and there is no reason to reproduce it.
//!
//! The window is measured from when a pass STARTED, exactly as the product measures
//! it: a pass that took twenty seconds does not thereby get a longer life than one
//! that took twenty milliseconds.
//!
//! ## Nothing about a pass can fail
//!
//! Every way a probe can go wrong is one of the four detection outcomes, so the pass
//! has no error to return: a missing binary is `Absent`, an unreadable version is
//! `VersionUnavailable`, and a probe that came apart is `ProbeFailed`. That last arm
//! is why each runtime is probed on its own task — a defect while probing one runtime
//! costs that runtime its answer, not the whole catalogue.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use silverhand_backend_core::identity_access::agents::runtime::RuntimeKind;
use silverhand_backend_core::identity_access::agents::runtime_catalog::{
    RuntimeCatalog, RuntimeCatalogDetector, RuntimeDetection, SELECTABLE_RUNTIMES,
};
use tokio::{sync::Mutex, task::JoinHandle};

use super::{
    machine::{DetectionPlatform as Platform, MachineFacts},
    search::{candidate_directories, resolve_executable},
    version::probe_version,
};

/// How long a pass answers for. The product's window, ported as-is.
pub const DETECTION_WINDOW: Duration = Duration::from_secs(30);

/// Reads the instant the window is measured against.
///
/// Monotonic on purpose: a wall clock that steps backwards or forwards would extend
/// or truncate a window that is about how stale a machine reading is, which has
/// nothing to do with what time it is.
pub trait DetectionClock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

pub struct SystemDetectionClock;

impl DetectionClock for SystemDetectionClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Detects the offered runtimes on the machine this process runs on.
pub struct MachineRuntimeDetector<C = SystemDetectionClock> {
    shared: Arc<Shared<C>>,
}

// Cloning a detector shares its ONE pass cache (the `Arc`), which is the whole point:
// every route handler that clones the router's state reads and refreshes the same
// thirty-second window. This is unconditional — the sole field is always cheaply
// cloneable — so it is spelled by hand rather than `#[derive(Clone)]`, which would
// wrongly demand `C: Clone` and lock out the non-`Clone` `SystemDetectionClock`.
impl<C> Clone for MachineRuntimeDetector<C> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

struct Shared<C> {
    machine: MachineFacts,
    window: Duration,
    clock: C,
    last_pass: Mutex<Option<CompletedPass>>,
}

struct CompletedPass {
    catalog: RuntimeCatalog,
    expires_at: Instant,
}

impl MachineRuntimeDetector<SystemDetectionClock> {
    /// The detector for the machine this process is on, with the product's window.
    pub fn for_this_process() -> Self {
        Self::new(
            MachineFacts::for_this_process(),
            DETECTION_WINDOW,
            SystemDetectionClock,
        )
    }
}

impl<C: DetectionClock> MachineRuntimeDetector<C> {
    pub fn new(machine: MachineFacts, window: Duration, clock: C) -> Self {
        Self {
            shared: Arc::new(Shared {
                machine,
                window,
                clock,
                last_pass: Mutex::new(None),
            }),
        }
    }
}

impl<C: DetectionClock> RuntimeCatalogDetector for MachineRuntimeDetector<C> {
    async fn detect_runtimes(&self) -> RuntimeCatalog {
        let shared = &self.shared;
        // Held across the pass: that is what makes concurrent first requests one
        // pass, and what makes a waiter read the fresh answer instead of starting
        // its own.
        let mut last_pass = shared.last_pass.lock().await;
        let started_at = shared.clock.now();
        if let Some(previous) = last_pass.as_ref()
            && previous.expires_at > started_at
        {
            return previous.catalog.clone();
        }
        let catalog = walk_machine(&shared.machine).await;
        *last_pass = Some(CompletedPass {
            catalog: catalog.clone(),
            expires_at: started_at + shared.window,
        });
        catalog
    }
}

/// One pass: assemble the search path once, then probe every offered runtime.
///
/// The pairing is positional and comes from the catalogue owner —
/// `SELECTABLE_RUNTIMES.map(..)` keeps each result at its runtime's index, and the
/// destructuring below states the arity the owner declares, so offering another
/// runtime stops compiling here instead of quietly dropping one.
async fn walk_machine(machine: &MachineFacts) -> RuntimeCatalog {
    let directories = Arc::new(candidate_directories(machine).await);
    let platform = machine.platform;
    let passes = SELECTABLE_RUNTIMES.map(|kind| {
        let directories = Arc::clone(&directories);
        tokio::spawn(async move { detect_one(kind, &directories, platform).await })
    });
    let [first, second, third, fourth, fifth, sixth, seventh] = passes;
    RuntimeCatalog::from_detections([
        concluded(first).await,
        concluded(second).await,
        concluded(third).await,
        concluded(fourth).await,
        concluded(fifth).await,
        concluded(sixth).await,
        concluded(seventh).await,
    ])
}

/// A probe that came apart concluded nothing — which is not the same as concluding
/// the runtime is absent, and the wire keeps them apart.
async fn concluded(pass: JoinHandle<RuntimeDetection>) -> RuntimeDetection {
    pass.await.unwrap_or(RuntimeDetection::ProbeFailed)
}

async fn detect_one(
    kind: RuntimeKind,
    directories: &[PathBuf],
    platform: Platform,
) -> RuntimeDetection {
    let Some(binary) = resolve_executable(kind, directories, platform).await else {
        return RuntimeDetection::Absent;
    };
    let path = binary.to_string_lossy().into_owned();
    match probe_version(&binary).await {
        Some(version) => RuntimeDetection::Resolved {
            binary: path,
            version,
        },
        // Resolved but silent: still installed, because the executable is the
        // evidence and the product treats it as usable.
        None => RuntimeDetection::VersionUnavailable { binary: path },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

    use tempfile::TempDir;

    use super::*;

    /// A clock a test moves by hand, so a window is proved by advancing time rather
    /// than by sleeping through it.
    struct TestClock {
        base: Instant,
        advanced_ms: AtomicU64,
    }

    impl TestClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                base: Instant::now(),
                advanced_ms: AtomicU64::new(0),
            })
        }

        fn advance(&self, by: Duration) {
            self.advanced_ms
                .fetch_add(u64::try_from(by.as_millis()).expect("advance fits"), SeqCst);
        }
    }

    impl DetectionClock for Arc<TestClock> {
        fn now(&self) -> Instant {
            self.base + Duration::from_millis(self.advanced_ms.load(SeqCst))
        }
    }

    /// A machine whose only search location is `directory`, and a clock the test
    /// drives.
    fn detector(
        directory: &std::path::Path,
        clock: Arc<TestClock>,
    ) -> MachineRuntimeDetector<Arc<TestClock>> {
        MachineRuntimeDetector::new(
            MachineFacts::only_searching(directory),
            DETECTION_WINDOW,
            clock,
        )
    }

    /// Install an executable named `name` that records every invocation in
    /// `probes.log` and then prints `prints`.
    #[cfg(unix)]
    fn install_executable(dir: &TempDir, name: &str, prints: &str) {
        use std::os::unix::fs::PermissionsExt;

        let log = dir.path().join("probes.log");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' {name} >> {log}\nprintf '%s' \"{prints}\"\n",
            log = log.display(),
        );
        let path = dir.path().join(name);
        std::fs::write(&path, script).expect("write the executable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
    }

    /// How many times any installed executable has been asked for its version.
    fn probe_count(dir: &TempDir) -> usize {
        std::fs::read_to_string(dir.path().join("probes.log"))
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }

    fn detection_of(catalog: &RuntimeCatalog, kind: RuntimeKind) -> RuntimeDetection {
        catalog
            .entries()
            .iter()
            .find(|entry| entry.kind == kind)
            .expect("every offered runtime is answered for")
            .detection
            .clone()
    }

    /// A pass answers for every offered runtime, in the catalogue's order, with what
    /// the machine actually has: a version when the binary printed one, the binary's
    /// own words when it printed something else, installed-without-a-version when it
    /// printed nothing, and absent when there is no executable at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pass_reports_what_the_machine_has_for_each_offered_runtime() {
        let dir = TempDir::new().expect("temp dir");
        install_executable(&dir, "claude", "claude 1.2.3 (build 9)");
        install_executable(&dir, "qodercli", "Qoder CLI internal");
        install_executable(&dir, "opencode", "");
        let clock = TestClock::new();

        let catalog = detector(dir.path(), clock).detect_runtimes().await;

        let listed: Vec<RuntimeKind> = catalog.entries().iter().map(|entry| entry.kind).collect();
        assert_eq!(listed, SELECTABLE_RUNTIMES.to_vec());
        assert_eq!(
            detection_of(&catalog, RuntimeKind::Claude),
            RuntimeDetection::Resolved {
                binary: dir.path().join("claude").to_string_lossy().into_owned(),
                version: "1.2.3".to_string(),
            }
        );
        assert_eq!(
            detection_of(&catalog, RuntimeKind::Qoder),
            RuntimeDetection::Resolved {
                binary: dir.path().join("qodercli").to_string_lossy().into_owned(),
                version: "Qoder CLI internal".to_string(),
            }
        );
        assert_eq!(
            detection_of(&catalog, RuntimeKind::Opencode),
            RuntimeDetection::VersionUnavailable {
                binary: dir.path().join("opencode").to_string_lossy().into_owned(),
            }
        );
        assert_eq!(
            detection_of(&catalog, RuntimeKind::Codex),
            RuntimeDetection::Absent
        );
    }

    /// Inside the window the machine is not touched again — the second read is the
    /// first read's answer.
    #[cfg(unix)]
    #[tokio::test]
    async fn two_reads_inside_the_window_walk_the_machine_once() {
        let dir = TempDir::new().expect("temp dir");
        install_executable(&dir, "claude", "1.0.0");
        let clock = TestClock::new();
        let detector = detector(dir.path(), clock.clone());

        let first = detector.detect_runtimes().await;
        clock.advance(Duration::from_millis(29_999));
        let second = detector.detect_runtimes().await;

        assert_eq!(first, second);
        assert_eq!(probe_count(&dir), 1);
    }

    /// At the window's end the machine is walked again, and the new answer replaces
    /// the old one. Installing the runtime between the two reads is how the test
    /// tells a fresh walk from a served cache: the cached answer CANNOT know about
    /// it.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_window_ends_and_the_machine_is_walked_again() {
        let dir = TempDir::new().expect("temp dir");
        let clock = TestClock::new();
        let detector = detector(dir.path(), clock.clone());

        let before = detector.detect_runtimes().await;
        assert_eq!(
            detection_of(&before, RuntimeKind::Claude),
            RuntimeDetection::Absent
        );

        install_executable(&dir, "claude", "3.0.1");
        // Still inside the window: a newly installed runtime is invisible, which is
        // what a cache MEANS.
        clock.advance(Duration::from_millis(29_999));
        assert_eq!(
            detection_of(&detector.detect_runtimes().await, RuntimeKind::Claude),
            RuntimeDetection::Absent
        );
        assert_eq!(probe_count(&dir), 0);

        // The window is over the instant it is no longer strictly in the future.
        clock.advance(Duration::from_millis(1));
        assert_eq!(
            detection_of(&detector.detect_runtimes().await, RuntimeKind::Claude),
            RuntimeDetection::Resolved {
                binary: dir.path().join("claude").to_string_lossy().into_owned(),
                version: "3.0.1".to_string(),
            }
        );
        assert_eq!(probe_count(&dir), 1);
    }

    /// Concurrent first reads share ONE walk. Without that, a burst of requests on a
    /// cold cache multiplies the most expensive thing this component does.
    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_first_reads_walk_the_machine_once() {
        let dir = TempDir::new().expect("temp dir");
        install_executable(&dir, "claude", "1.0.0");
        let clock = TestClock::new();
        let detector = detector(dir.path(), clock);

        let (first, second, third, fourth) = tokio::join!(
            detector.detect_runtimes(),
            detector.detect_runtimes(),
            detector.detect_runtimes(),
            detector.detect_runtimes(),
        );

        assert_eq!(probe_count(&dir), 1);
        assert_eq!(first, second);
        assert_eq!(second, third);
        assert_eq!(third, fourth);
    }

    /// A clone is the same detector: it reads the window the original filled, and
    /// cannot start a walk of its own inside it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_clone_shares_the_window() {
        let dir = TempDir::new().expect("temp dir");
        install_executable(&dir, "claude", "1.0.0");
        let clock = TestClock::new();
        let detector = detector(dir.path(), clock);
        let handed_to_a_factory = detector.clone();

        let original = detector.detect_runtimes().await;
        let clone = handed_to_a_factory.detect_runtimes().await;

        assert_eq!(original, clone);
        assert_eq!(probe_count(&dir), 1);
    }

    /// A probe that came apart is reported as such — never as a machine without the
    /// runtime, and never by taking the pass down with it.
    #[tokio::test]
    async fn a_pass_that_panicked_concluded_nothing_rather_than_absence() {
        let came_apart = tokio::spawn(async { panic!("a probe defect") });
        assert_eq!(concluded(came_apart).await, RuntimeDetection::ProbeFailed);
    }
}
