//! Dependency direction and pure-policy architecture contracts.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

/// Driver and runtime families. `backend-core`'s linked library stays free of
/// side-effecting infrastructure through its whole transitive normal closure.
/// Pure data-format libraries are deliberately outside this list: the accepted
/// internal-profile MCP capability owns canonical JSON config admission in core.
const DRIVER_WIRE_RUNTIME: &[&str] = &[
    "axum",
    "http",
    "http-body",
    "hyper",
    "libsqlite3-sys",
    "sqlx",
    "sqlx-core",
    "sqlx-sqlite",
    "tokio",
    "tokio-tungstenite",
    "tower",
    "tower-http",
    "tracing",
    "tracing-subscriber",
    "ts-rs",
    "tungstenite",
];

/// Async runtimes, servers and DB drivers. A build script is allowed to read a
/// pinned authority and generate tables, so a wire format alone says nothing
/// about it; reaching one of these families says it does something other than
/// generation. Build closures are held to this list instead of the linked one.
const RUNTIME_AND_DRIVER: &[&str] = &[
    "axum",
    "hyper",
    "libsqlite3-sys",
    "sqlx",
    "sqlx-core",
    "sqlx-sqlite",
    "tokio",
    "tokio-tungstenite",
    "tower",
    "tower-http",
    "tungstenite",
];

/// Namespaces with a declaration-only ownership contract (not all directories).
const DIRECTORY_NAMESPACES: &[(&str, &str)] = &[
    ("backend-filesystem", "host_runtime"),
    ("backend-server", "scheduling"),
    ("backend-server", "embedded_bootstrap"),
    ("backend-server", "workspace_data_plane"),
    ("backend-core", "embedded_bootstrap"),
    ("backend-core", "workspace_data_plane"),
    ("backend-sqlite", "embedded_bootstrap"),
    ("backend-sqlite", "workspace_data_plane"),
    ("backend-sqlite", "application_orchestration"),
    ("backend-core", "identity_access"),
    ("backend-core", "viewer_read_model"),
    ("backend-core", "foundation"),
    ("backend-core", "conversation_messaging"),
    ("backend-core", "application_orchestration"),
    ("backend-sqlite", "viewer_read_model"),
    ("backend-sqlite", "foundation"),
    ("backend-sqlite", "conversation_messaging"),
    ("backend-sqlite", "identity_access"),
];

fn rust_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("rust workspace root")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Cargo's own resolution of the workspace, read once per gate.
///
/// A hand-written manifest or lock parser cannot see target-specific edges,
/// `package = ` renames, feature-gated optional edges, or two versions of one
/// name, so every dependency assertion below reads the resolved graph instead of
/// re-deriving one. `--locked --offline` keeps the lock file untouched.
struct ResolvedGraph {
    /// Package id to package name; ids are the only stable identity, because two
    /// versions of one crate share a name.
    names: BTreeMap<String, String>,
    /// Package id to dependency-kind (`normal`, `build`, `dev`) to package ids,
    /// merged across every target so a `cfg(...)` edge cannot hide.
    edges: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    /// Workspace member name to package id.
    members: BTreeMap<String, String>,
}

fn resolved_graph() -> ResolvedGraph {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["metadata", "--locked", "--offline", "--format-version", "1"])
        .current_dir(rust_root())
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("cargo metadata json");

    let mut names = BTreeMap::new();
    for package in metadata["packages"].as_array().expect("packages") {
        names.insert(
            package["id"].as_str().expect("package id").to_owned(),
            package["name"].as_str().expect("package name").to_owned(),
        );
    }

    let mut members = BTreeMap::new();
    for id in metadata["workspace_members"]
        .as_array()
        .expect("workspace members")
    {
        let id = id.as_str().expect("member id").to_owned();
        members.insert(names[&id].clone(), id);
    }

    let mut edges: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    for node in metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve nodes")
    {
        let id = node["id"].as_str().expect("node id").to_owned();
        let per_kind = edges.entry(id).or_default();
        for dependency in node["deps"].as_array().expect("node deps") {
            let target = dependency["pkg"].as_str().expect("dep pkg").to_owned();
            for dependency_kind in dependency["dep_kinds"].as_array().expect("dep kinds") {
                // A normal dependency carries no `kind` at all.
                let kind = dependency_kind["kind"].as_str().unwrap_or("normal");
                per_kind
                    .entry(kind.to_owned())
                    .or_default()
                    .insert(target.clone());
            }
        }
    }

    ResolvedGraph {
        names,
        edges,
        members,
    }
}

impl ResolvedGraph {
    fn member(&self, package: &str) -> &str {
        self.members
            .get(package)
            .unwrap_or_else(|| panic!("{package} is not a workspace member"))
    }

    fn kind(&self, id: &str, kind: &str) -> BTreeSet<String> {
        self.edges
            .get(id)
            .and_then(|per_kind| per_kind.get(kind))
            .cloned()
            .unwrap_or_default()
    }

    /// Everything cargo links into `package`'s own library: its normal edges,
    /// then the normal edges of everything those reach. A dev edge is never
    /// transitive, and a dependency's build script feeds a separate binary that
    /// is never linked into its consumer, so neither kind enters this closure.
    fn linked_closure(&self, package: &str) -> BTreeSet<String> {
        self.closure(self.kind(self.member(package), "normal"), &["normal"])
    }

    /// Everything cargo compiles to produce `package`'s build script: its build
    /// edges, then the normal and build edges of everything those reach. None of
    /// it is linked into the library, so this answers a different question than
    /// [`Self::linked_closure`] and is held to a different family list.
    fn build_closure(&self, package: &str) -> BTreeSet<String> {
        self.closure(
            self.kind(self.member(package), "build"),
            &["normal", "build"],
        )
    }

    fn closure(&self, roots: BTreeSet<String>, follow: &[&str]) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<String> = roots.into_iter().collect();
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            for kind in follow {
                queue.extend(self.kind(&id, kind));
            }
        }
        seen.iter().map(|id| self.names[id].clone()).collect()
    }

    /// Test-only edges of a workspace member, by package name.
    fn dev_dependencies(&self, package: &str) -> BTreeSet<String> {
        self.kind(self.member(package), "dev")
            .iter()
            .map(|id| self.names[id].clone())
            .collect()
    }
}

fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_block = false;
    while let Some(character) = chars.next() {
        if in_block {
            if character == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block = false;
            }
            continue;
        }
        if character == '/' {
            match chars.peek() {
                Some('/') => {
                    for skipped in chars.by_ref() {
                        if skipped == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                    continue;
                }
                Some('*') => {
                    chars.next();
                    in_block = true;
                    continue;
                }
                _ => {}
            }
        }
        out.push(character);
    }
    out
}

fn rust_sources(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn lane_source_paths(crate_dir: &str, namespace: &str) -> Vec<PathBuf> {
    rust_sources(
        &rust_root()
            .join("crates")
            .join(crate_dir)
            .join("src")
            .join(namespace),
    )
}

/// Whether the source names `identifier` as a WHOLE identifier.
///
/// A bare substring cannot be used for a namespace name that is a suffix of an
/// unrelated one — `workspace_cold_read` is a different module, not a second
/// declaration of `cold_read` — and matching a `::`-delimited path segment cannot
/// either, because a declaration or re-export of a namespace need not carry a `::`.
fn names_identifier(source: &str, identifier: &str) -> bool {
    let is_ident = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    source.match_indices(identifier).any(|(start, matched)| {
        let before_is_ident = start > 0 && is_ident(source.as_bytes()[start - 1]);
        let after = start + matched.len();
        let after_is_ident = after < source.len() && is_ident(source.as_bytes()[after]);
        !before_is_ident && !after_is_ident
    })
}

/// Drops leading attributes, so `#[cfg(test)] mod x;` parses as the declaration it
/// is, and a crate root's own `#![forbid(..)]` does not hide the first one.
///
/// Terminates each attribute at the first `]`, which is sufficient for attributes
/// carrying paths, strings and nested parens; an attribute containing a `]` inside a
/// string would end the scan early and can only make this predicate MISS, never
/// invent, a declaration.
fn strip_attributes(item: &str) -> &str {
    let mut rest = item.trim_start();
    loop {
        let opened = rest.strip_prefix("#![").or_else(|| rest.strip_prefix("#["));
        let Some(attribute) = opened else {
            return rest;
        };
        let Some(end) = attribute.find(']') else {
            return rest;
        };
        rest = attribute[end + 1..].trim_start();
    }
}

/// Splits a leading visibility off an item, reporting whether the item is EXPORTED
/// beyond its own file.
///
/// `pub`, `pub(crate)`, `pub(super)` and `pub(in path)` are four spellings of one
/// property, and matching text per spelling is how `pub(crate) use` slipped past a
/// gate whose prose claimed to catch re-exports.
fn split_visibility(item: &str) -> (bool, &str) {
    let Some(rest) = item.strip_prefix("pub") else {
        return (false, item);
    };
    if let Some(restriction) = rest.strip_prefix('(') {
        return match restriction.find(')') {
            Some(end) => (true, restriction[end + 1..].trim_start()),
            None => (false, item),
        };
    }
    // `pub` must be a whole token: `published` is not a visibility.
    if rest.starts_with(char::is_whitespace) {
        return (true, rest.trim_start());
    }
    (false, item)
}

#[test]
fn g1_core_carries_no_driver_wire_or_runtime_dependency() {
    let graph = resolved_graph();

    let linked = graph.linked_closure("silverhand-backend-core");
    let in_library: Vec<&str> = DRIVER_WIRE_RUNTIME
        .iter()
        .copied()
        .filter(|package| linked.contains(*package))
        .collect();
    assert!(
        in_library.is_empty(),
        "backend-core linked closure reached {in_library:?}"
    );

    let build = graph.build_closure("silverhand-backend-core");
    let in_build: Vec<&str> = RUNTIME_AND_DRIVER
        .iter()
        .copied()
        .filter(|package| build.contains(*package))
        .collect();
    assert!(
        in_build.is_empty(),
        "backend-core build closure reached {in_build:?}"
    );
}

#[test]
fn g1_the_persistence_and_wire_crates_keep_their_direction() {
    let graph = resolved_graph();
    assert!(
        !graph
            .linked_closure("silverhand-backend-sqlite")
            .contains("silverhand-backend-server"),
        "backend-sqlite must not depend on the wire crate"
    );
    assert!(
        !graph
            .linked_closure("silverhand-backend-server")
            .contains("silverhand-backend-sqlite"),
        "backend-server production code must not be able to name the SQLite adapter"
    );
    assert!(
        graph
            .dev_dependencies("silverhand-backend-server")
            .contains("silverhand-backend-sqlite"),
        "the SQLite adapter is a backend-server test-only dependency"
    );

    // The filesystem adapter is a leaf: its linked closure reaches only backend-core
    // (+ tokio), so it can name neither the wire crate nor the SQLite adapter. This
    // pins the new adapter's acyclicity.
    //
    // Honest scope: the reverse edge — backend-server naming backend-filesystem — DOES
    // exist as #task:2200's approved typed local-alias capability, and is deliberately
    // NOT fenced here. So this gate pins adapter acyclicity, not the full
    // "persistence/wire direction" its title claims; that one approved server→filesystem
    // edge is a bounded exception, not a hole.
    let filesystem = graph.linked_closure("silverhand-backend-filesystem");
    assert!(
        !filesystem.contains("silverhand-backend-server"),
        "backend-filesystem must not depend on the wire crate"
    );
    assert!(
        !filesystem.contains("silverhand-backend-sqlite"),
        "backend-filesystem must not depend on the SQLite adapter"
    );
}

#[test]
fn g2_background_policy_names_no_timer_or_driver() {
    // Residual guard only: aliases, macros and re-exports can bypass a source
    // scan. The compile-enforced carrier is G1 plus core's manifest.
    //
    // A crate or module path is a reach wherever it appears, so it is matched as
    // a substring. A bare type or function name only catches an aliased reach,
    // and a domain type may legitimately end in one — `ActivityInstant` is a
    // validated stamp, not a clock — so those are matched as whole identifiers.
    const FORBIDDEN_PATHS: &[&str] = &["tokio", "std::time", "axum", "sqlx"];
    const FORBIDDEN_NAMES: &[&str] = &["Instant", "SystemTime", "sleep", "spawn"];
    let policy_paths = [
        rust_root().join("crates/backend-core/src/host_runtime/agent_presence.rs"),
        rust_root().join("crates/backend-core/src/viewer_read_model/presence_grace.rs"),
    ];
    for path in policy_paths {
        let source = strip_comments(&read(&path));
        for forbidden in FORBIDDEN_PATHS {
            assert!(
                !source.contains(forbidden),
                "{} names {forbidden}: policy decisions stay timer-free",
                path.display()
            );
        }
        for forbidden in FORBIDDEN_NAMES {
            assert!(
                !names_identifier(&source, forbidden),
                "{} names {forbidden}: policy decisions stay timer-free",
                path.display()
            );
        }
    }
}

#[test]
fn g2_background_timers_carry_no_http_surface() {
    let paths = lane_source_paths("backend-server", "host_runtime/host_agent_background");
    assert!(!paths.is_empty(), "background namespace must be scanned");
    for path in paths {
        let source = strip_comments(&read(&path));
        for forbidden in ["axum", "Router", "StatusCode"] {
            assert!(
                !source.contains(forbidden),
                "{} names {forbidden}: timers are not an HTTP surface",
                path.display()
            );
        }
    }
}

#[test]
fn g5_namespaces_declare_and_never_carry_business() {
    for (crate_dir, namespace) in DIRECTORY_NAMESPACES {
        let path = rust_root()
            .join("crates")
            .join(crate_dir)
            .join("src")
            .join(namespace)
            .join("mod.rs");
        // Item-based for the same reason as G4: a braced re-export group and a
        // visibility qualifier are legal, and a function body is not.
        for item in strip_comments(&read(&path)).split(';') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (_, rest) = split_visibility(strip_attributes(item));
            assert!(
                rest.starts_with("mod ") || rest.starts_with("use "),
                "{} carries `{item}`: a namespace declares and re-exports only",
                path.display()
            );
        }
    }
}

/// G6 — the capability namespace decides, and never acquires.
///
/// A pure decision over already-read facts is what lets one rule serve a face
/// that reads before an independent write and a face that must read under the
/// write lock it commits in. The two properties that make it pure are carried
/// differently, and only one of them needs a fence:
///
///  - "holds no driver" is already compile-enforced — `backend-core`'s production
///    closure reaches no driver, wire or runtime crate (G1);
///  - "acquires nothing" is a SIGNATURE property: these functions take facts and
///    return a verdict, so a rule that wanted to read would have to change its
///    signature and every call site. This gate is the tripwire against the first
///    step towards that — declaring a port or going async inside the namespace.
///
/// A LEXICAL fence, with the same accounting as G3: it guards the named boundary
/// "capability acquires no facts", it is removed when a compile-level carrier
/// exists (a sealed fact-provider trait owned outside this namespace would be
/// one), and it is owned by this lane.
#[test]
fn g6_capability_decides_over_facts_and_declares_no_port() {
    // The `capability` decision namespace relocated under the C1 `identity_access` container
    // (#task:2277); this behavioral fence follows it to its new path. The invariant is
    // unchanged — a capability decides over facts it was handed and declares no port.
    let paths = lane_source_paths("backend-core", "identity_access/capability");
    assert!(
        !paths.is_empty(),
        "expected the capability namespace to carry its leaves, found {paths:?}"
    );
    for path in paths {
        let source = strip_comments(&read(&path));
        for forbidden in ["async fn", "impl Future", "-> impl Future", "trait "] {
            assert!(
                !source.contains(forbidden),
                "{} carries `{forbidden}`: a capability decides over facts it was handed",
                path.display()
            );
        }
    }
}
