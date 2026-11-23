//! Where this machine's runtime executables might be, and which one to use.
//!
//! ## The search order IS the product's answer
//!
//! A runtime resolves to the FIRST executable found while walking the sources in
//! order, so the order below is a behavioural contract, not an implementation
//! detail. Two sources are load-bearing enough to name:
//!
//!  - **`PATH` first, whole.** Everything else is a fallback for the many ways a
//!    GUI-launched process inherits a `PATH` that a login shell would not have.
//!  - **Version-manager shims last.** A shim answers `--version` by starting a whole
//!    Node toolchain; if a real binary is on `PATH`, it wins.
//!
//! ## Per-runtime hard candidates come before any directory
//!
//! One runtime has locations that beat a `PATH` lookup outright: the codex binary
//! shipped inside a macOS app bundle starts in a fraction of the time the npm
//! wrapper takes, because the wrapper is a Node script that re-spawns the real
//! binary after a cold dyld start. Those candidates are probed before the search
//! path, in the order listed, and no version equality with the npm CLI is required —
//! installing the app IS the acceptance.
//!
//! ## Executability is the OS's answer about THIS process
//!
//! A candidate counts when `stat` says it is a regular file AND the OS says this
//! process may execute it — the two questions the product asks, in that order.
//!
//! Neither implies the other, which is why both are asked. A permission-bit test is
//! a DIFFERENT question: a file owned by this process with mode `010` has an execute
//! bit set and cannot be executed by it, so a bit test resolves a candidate the
//! product would have skipped, and the runtime is then reported installed on a
//! machine where nothing can start it. And `X_OK` alone answers yes for a directory,
//! where the bit means searchable, so dropping the `is_file` half would resolve a
//! directory named after a runtime.

use std::path::{Path, PathBuf};

use silverhand_backend_core::identity_access::agents::runtime::RuntimeKind;

use super::machine::{DetectionPlatform, MachineFacts};
use super::version::run_command;
use std::time::Duration;

/// The executable name each runtime is looked up under. Not the runtime's stored
/// spelling: two of them differ (`cursor-agent`, `qodercli`), which is exactly why
/// this mapping exists rather than reusing [`RuntimeKind::as_str`].
fn executable_name(kind: RuntimeKind) -> &'static str {
    match kind {
        RuntimeKind::Claude => "claude",
        RuntimeKind::Codex => "codex",
        RuntimeKind::Cursor => "cursor-agent",
        RuntimeKind::Gemini => "gemini",
        RuntimeKind::Qoder => "qodercli",
        RuntimeKind::Opencode => "opencode",
        RuntimeKind::Pi => "pi",
        RuntimeKind::Grok => "grok",
        RuntimeKind::Antigravity => "agy",
    }
}

/// Locations that beat the search path for one runtime on one platform, in
/// resolution priority. The standalone `Codex.app` is the legacy bundle, kept for
/// machines that still have it.
fn hard_candidates(kind: RuntimeKind, platform: DetectionPlatform) -> &'static [&'static str] {
    match (kind, platform) {
        (RuntimeKind::Codex, DetectionPlatform::MacOs) => &[
            "/Applications/ChatGPT.app/Contents/Resources/codex",
            "/Applications/Codex.app/Contents/Resources/codex",
        ],
        _ => &[],
    }
}

/// `path_helper` is a macOS facility that prints the system-wide `PATH` assembled
/// from `/etc/paths` and `/etc/paths.d`. Short budget: it reads two small files.
const PATH_HELPER_TIMEOUT: Duration = Duration::from_secs(2);

/// A login shell is asked for its `PATH` because a desktop-launched process does not
/// get one. Its startup files are arbitrary user code, so the budget is bounded.
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// Markers around the shell's answer, so unrelated startup chatter is not parsed as
/// a search path.
const SHELL_PATH_START: &str = "__silverhand_path_start__";
const SHELL_PATH_END: &str = "__silverhand_path_end__";

/// Every directory to look in, in resolution order, each appearing once.
pub(super) async fn candidate_directories(facts: &MachineFacts) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    directories.extend(split_search_path(facts.path_env.as_deref()));
    directories.extend(login_shell_directories(facts.login_shell.as_deref()).await);
    directories.extend(path_helper_directories(facts.path_helper.as_deref()).await);
    if facts.include_default_directories
        && let Some(home) = facts.home.as_deref()
    {
        directories.extend(common_directories(home));
        directories.extend(version_manager_directories(home).await);
    }
    deduplicate(directories)
}

/// The executable for `kind`, or `None` when the machine has none.
pub(super) async fn resolve_executable(
    kind: RuntimeKind,
    directories: &[PathBuf],
    platform: DetectionPlatform,
) -> Option<PathBuf> {
    for candidate in hard_candidates(kind, platform) {
        let candidate = Path::new(candidate);
        if is_executable_file(candidate).await {
            return Some(candidate.to_path_buf());
        }
    }
    let name = executable_name(kind);
    for directory in directories {
        let candidate = directory.join(name);
        if is_executable_file(&candidate).await {
            return Some(candidate);
        }
    }
    None
}

/// Split a `PATH`-shaped value. Entries are trimmed and empty ones dropped, so a
/// trailing separator does not turn into the process's working directory.
fn split_search_path(value: Option<&str>) -> Vec<PathBuf> {
    let Some(value) = value else {
        return Vec::new();
    };
    value
        .split(':')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The `PATH` an interactive login shell would have. A shell that fails, or is
/// killed at the deadline, contributes nothing — never a partial answer.
async fn login_shell_directories(shell: Option<&Path>) -> Vec<PathBuf> {
    let Some(shell) = shell else {
        return Vec::new();
    };
    let script = format!(
        "printf '%s\\n' {SHELL_PATH_START}; printf '%s\\n' \"$PATH\"; printf '%s\\n' \
         {SHELL_PATH_END}"
    );
    let Some(result) = run_command(shell, &["-ilc", &script], LOGIN_SHELL_TIMEOUT).await else {
        return Vec::new();
    };
    if result.code != Some(0) {
        return Vec::new();
    }
    let Some(start) = result.stdout.find(SHELL_PATH_START) else {
        return Vec::new();
    };
    let Some(end) = result.stdout.find(SHELL_PATH_END) else {
        return Vec::new();
    };
    let start = start + SHELL_PATH_START.len();
    if end <= start {
        return Vec::new();
    }
    let body = result.stdout[start..end].trim();
    split_search_path(body.lines().find(|line| !line.is_empty()))
}

/// The system-wide `PATH` macOS assembles from `/etc/paths`.
async fn path_helper_directories(path_helper: Option<&Path>) -> Vec<PathBuf> {
    let Some(path_helper) = path_helper else {
        return Vec::new();
    };
    let Some(result) = run_command(path_helper, &["-s"], PATH_HELPER_TIMEOUT).await else {
        return Vec::new();
    };
    if result.code != Some(0) {
        return Vec::new();
    }
    split_search_path(quoted_path_assignment(&result.stdout))
}

/// Pull `<VALUE>` out of the shell assignment `PATH="<VALUE>"; export PATH;` the
/// helper prints.
///
/// Substring, not line-anchored — deliberately, because that is what the product
/// does, and the helper always prints `PATH` before the other variables it sets, so
/// the leftmost match is the one meant. The visible consequence is that output which
/// set only `MANPATH` would be read as a search path; the value would be a list of
/// directories with no runtime in them, and inventing an anchor here would be a
/// behaviour change on input the helper does not produce.
fn quoted_path_assignment(output: &str) -> Option<&str> {
    let assignment = output.find("PATH=\"")? + "PATH=\"".len();
    let rest = &output[assignment..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Package-manager and system locations, in the product's order.
fn common_directories(home: &Path) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = [
        ".local/bin",
        ".npm-global/bin",
        ".npm/bin",
        ".bun/bin",
        "Library/pnpm",
        ".yarn/bin",
        ".config/yarn/global/node_modules/.bin",
    ]
    .iter()
    .map(|suffix| home.join(suffix))
    .collect();
    directories.extend(
        [
            "/opt/homebrew/bin",
            "/opt/homebrew/sbin",
            "/usr/local/bin",
            "/usr/local/sbin",
            "/opt/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        .iter()
        .map(PathBuf::from),
    );
    directories
}

/// Version-manager locations: the fixed "current version" shims first, then every
/// installed version's own bin directory.
async fn version_manager_directories(home: &Path) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = [
        ".volta/bin",
        ".asdf/shims",
        ".local/share/mise/shims",
        ".nvm/current/bin",
        ".fnm/aliases/default/bin",
    ]
    .iter()
    .map(|suffix| home.join(suffix))
    .collect();
    directories.extend(child_directories(&home.join(".nvm/versions/node"), "bin").await);
    directories
        .extend(child_directories(&home.join(".fnm/node-versions"), "installation/bin").await);
    directories
}

/// `parent/<child>/<suffix>` for every directory entry of `parent`.
///
/// The entry's own type decides, without following symlinks — the product reads the
/// directory entry, so a symlinked version directory is skipped there too, and this
/// must not become the one place that resolves them.
async fn child_directories(parent: &Path, suffix: &str) -> Vec<PathBuf> {
    let Ok(mut entries) = tokio::fs::read_dir(parent).await else {
        return Vec::new();
    };
    let mut directories = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        match entry.file_type().await {
            Ok(file_type) if file_type.is_dir() => {
                directories.push(entry.path().join(suffix));
            }
            _ => {}
        }
    }
    directories
}

fn deduplicate(directories: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    directories
        .into_iter()
        .filter(|directory| seen.insert(directory.clone()))
        .collect()
}

/// Whether the candidate is a regular file this process may execute.
///
/// Both questions are asked on one blocking hop: they are about the same file, and a
/// candidate can sit on a mount where either of them waits on I/O.
async fn is_executable_file(path: &Path) -> bool {
    let candidate = path.to_path_buf();
    tokio::task::spawn_blocking(move || is_regular_file(&candidate) && may_execute(&candidate))
        .await
        .unwrap_or(false)
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Asks the OS whether this process may execute the path.
///
/// `EACCESS` asks under the identity a spawn would actually run as (the effective
/// ids) rather than the invoking user's. The two agree for any process that is not
/// setuid — which this is not — and asking the spawn's own question is the one that
/// stays correct if that ever changes.
#[cfg(unix)]
fn may_execute(path: &Path) -> bool {
    rustix::fs::accessat(
        rustix::fs::CWD,
        path,
        rustix::fs::Access::EXEC_OK,
        rustix::fs::AtFlags::EACCESS,
    )
    .is_ok()
}

/// No execute permission to consult. The version probe is then the real test of
/// whether the candidate runs — a platform that spells executables differently needs
/// its own resolution rules, not a lenient check here.
#[cfg(not(unix))]
fn may_execute(_path: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_path_contributes_its_entries_in_order_without_empty_ones() {
        assert_eq!(
            split_search_path(Some("/first: /second :/first:")),
            vec![
                PathBuf::from("/first"),
                PathBuf::from("/second"),
                PathBuf::from("/first"),
            ]
        );
        assert_eq!(split_search_path(Some("")), Vec::<PathBuf>::new());
        assert_eq!(split_search_path(None), Vec::<PathBuf>::new());
    }

    #[test]
    fn the_helpers_assignment_is_read_the_way_the_product_reads_it() {
        // The real thing: `PATH` first, other variables after it.
        assert_eq!(
            quoted_path_assignment(
                "PATH=\"/usr/bin:/bin\"; export PATH;\nMANPATH=\"/usr/share/man\"; export MANPATH;\n"
            ),
            Some("/usr/bin:/bin")
        );
        // The documented consequence of matching a substring, pinned so a future
        // anchor is a deliberate change rather than a silent one.
        assert_eq!(
            quoted_path_assignment("MANPATH=\"/x\"; export MANPATH;"),
            Some("/x")
        );
        assert_eq!(quoted_path_assignment("PATH=\"unterminated"), None);
        assert_eq!(quoted_path_assignment("nothing to read"), None);
    }

    /// The order is the contract: the process's own `PATH` first, home-relative
    /// package-manager directories next, system directories after them, and
    /// version-manager shims last.
    #[tokio::test]
    async fn the_search_order_puts_path_first_and_version_manager_shims_last() {
        let facts = MachineFacts {
            platform: DetectionPlatform::Other,
            home: Some(PathBuf::from("/home/someone")),
            path_env: Some("/from/path".to_string()),
            login_shell: None,
            path_helper: None,
            include_default_directories: true,
        };
        let directories = candidate_directories(&facts).await;
        assert_eq!(directories.first(), Some(&PathBuf::from("/from/path")));
        assert_eq!(
            directories.last(),
            Some(&PathBuf::from("/home/someone/.fnm/aliases/default/bin"))
        );
        let index = |needle: &str| {
            directories
                .iter()
                .position(|directory| directory == Path::new(needle))
                .unwrap_or_else(|| panic!("{needle} is not a candidate"))
        };
        assert!(index("/home/someone/.local/bin") < index("/usr/bin"));
        assert!(index("/usr/bin") < index("/home/someone/.volta/bin"));
    }

    /// Nothing outside `PATH` when the defaults are off: a machine's real
    /// installations must not leak into an answer that was asked to be narrow.
    #[tokio::test]
    async fn defaults_can_be_left_out_entirely() {
        let facts = MachineFacts {
            platform: DetectionPlatform::Other,
            home: Some(PathBuf::from("/home/someone")),
            path_env: Some("/only/this".to_string()),
            login_shell: None,
            path_helper: None,
            include_default_directories: false,
        };
        assert_eq!(
            candidate_directories(&facts).await,
            vec![PathBuf::from("/only/this")]
        );
    }

    /// A directory reached twice is searched once, and keeps its FIRST position —
    /// deduplicating in the other direction would silently reorder resolution.
    #[tokio::test]
    async fn a_repeated_directory_keeps_its_first_position() {
        let facts = MachineFacts {
            platform: DetectionPlatform::Other,
            home: None,
            path_env: Some("/a:/b:/a".to_string()),
            login_shell: None,
            path_helper: None,
            include_default_directories: true,
        };
        assert_eq!(
            candidate_directories(&facts).await,
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }

    /// Two runtimes are not looked up under their stored spelling, which is the whole
    /// reason this mapping is not [`RuntimeKind::as_str`].
    #[test]
    fn the_executable_name_differs_from_the_stored_spelling_where_the_product_says_so() {
        assert_eq!(executable_name(RuntimeKind::Qoder), "qodercli");
        assert_eq!(executable_name(RuntimeKind::Cursor), "cursor-agent");
        for same in [
            RuntimeKind::Claude,
            RuntimeKind::Codex,
            RuntimeKind::Gemini,
            RuntimeKind::Opencode,
        ] {
            assert_eq!(executable_name(same), same.as_str());
        }
    }

    /// The bundle candidates are macOS-only and ordered; every other pair has none,
    /// so no runtime silently gains a hard-coded location.
    #[test]
    fn only_codex_on_macos_has_locations_that_beat_the_search_path() {
        assert_eq!(
            hard_candidates(RuntimeKind::Codex, DetectionPlatform::MacOs),
            [
                "/Applications/ChatGPT.app/Contents/Resources/codex",
                "/Applications/Codex.app/Contents/Resources/codex",
            ]
        );
        assert!(hard_candidates(RuntimeKind::Codex, DetectionPlatform::Other).is_empty());
        for kind in RuntimeKind::ALL {
            if kind == RuntimeKind::Codex {
                continue;
            }
            assert!(hard_candidates(kind, DetectionPlatform::MacOs).is_empty());
            assert!(hard_candidates(kind, DetectionPlatform::Other).is_empty());
        }
    }

    /// Executability is the OS's answer about THIS process, not a bit pattern.
    ///
    /// The refused candidate is an ordinary file with mode `010`: it HAS an execute
    /// bit, and its owner — this process — may not run it. So it is not an answer,
    /// and a later candidate the OS does say yes about is. A `mode & 0o111` check
    /// resolves the refused one and never reaches the second assertion.
    ///
    /// Nothing here is executed: the refused file is only asked about, and the
    /// control is a symlink to an executable the machine already has.
    #[cfg(unix)]
    #[tokio::test]
    async fn resolution_skips_a_candidate_this_process_may_not_execute() {
        use std::os::unix::fs::PermissionsExt;

        // For root the access check succeeds on any execute bit, so the distinction
        // this gate is about does not exist under that identity.
        if rustix::process::geteuid().is_root() {
            return;
        }
        let machine = tempfile::TempDir::new().expect("temp dir");
        let refusing = machine.path().join("bin-we-cannot-use");
        let runnable = machine.path().join("bin-we-can");
        std::fs::create_dir(&refusing).expect("a candidate directory");
        std::fs::create_dir(&runnable).expect("a candidate directory");

        let refused = refusing.join("claude");
        std::fs::write(&refused, "never executed by this test").expect("write the file");
        std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(0o010))
            .expect("an execute bit its owner cannot use");

        assert_eq!(
            resolve_executable(
                RuntimeKind::Claude,
                std::slice::from_ref(&refusing),
                DetectionPlatform::Other
            )
            .await,
            None,
            "a file this process may not execute is not a runtime the machine has"
        );

        let usable = runnable.join("claude");
        std::os::unix::fs::symlink(A_SYSTEM_EXECUTABLE, &usable).expect("link the control");
        assert_eq!(
            resolve_executable(
                RuntimeKind::Claude,
                &[refusing, runnable],
                DetectionPlatform::Other
            )
            .await,
            Some(usable),
            "the search continues past the refusal to the candidate that can run"
        );
    }

    /// An executable the machine already has, used as the positive control — nothing
    /// in these tests is executed, so the control must be something the OS already
    /// answers yes about.
    #[cfg(unix)]
    const A_SYSTEM_EXECUTABLE: &str = "/bin/sh";

    /// `X_OK` on a directory answers yes, because there the bit means searchable. The
    /// file half of the question is what keeps a directory named after a runtime from
    /// being resolved as one.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_directory_named_after_a_runtime_is_not_an_executable() {
        let machine = tempfile::TempDir::new().expect("temp dir");
        let in_the_way = machine.path().join("claude");
        std::fs::create_dir(&in_the_way).expect("a directory where a binary would be");

        assert!(
            may_execute(&in_the_way),
            "the OS calls a searchable directory executable"
        );
        assert_eq!(
            resolve_executable(
                RuntimeKind::Claude,
                &[machine.path().to_path_buf()],
                DetectionPlatform::Other
            )
            .await,
            None
        );
    }
}
