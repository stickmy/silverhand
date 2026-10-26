//! Independent backend ownership constraints retained after the migration map retired.
//! These are architecture guards, not a module-by-module migration inventory.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

fn rust_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("rust workspace root")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Top-level `mod NAME;` declarations of a source file, read through a real parser so a
/// `#[cfg(test)]`-gated declaration still counts (it is still a declared module) and a
/// commented-out or string-embedded `mod` never does.
fn module_declarations(source: &str) -> Vec<(String, bool)> {
    let parsed = syn::parse_file(source).expect("a parseable Rust source file");
    let mut mods = Vec::new();
    for item in &parsed.items {
        if let syn::Item::Mod(item) = item {
            // An inline `mod foo { .. }` is not a crate-root file/dir declaration; only a
            // `mod foo;` (no content) names an external module the map must classify.
            if item.content.is_none() {
                let cfg_test = item.attrs.iter().any(is_cfg_test);
                mods.push((item.ident.to_string(), cfg_test));
            }
        }
    }
    mods
}

fn is_cfg_test(attr: &syn::Attribute) -> bool {
    if !attr.path().is_ident("cfg") {
        return false;
    }
    let mut found = false;
    let _ = attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("test") {
            found = true;
        }
        Ok(())
    });
    found
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

fn workspace_crates() -> Vec<String> {
    let manifest = read(&rust_root().join("Cargo.toml"));
    let members = workspace_members(&manifest);
    let mut from_manifest: Vec<String> = members
        .iter()
        .filter_map(|m| m.strip_prefix("crates/").map(|c| c.to_owned()))
        .collect();
    from_manifest.sort();
    from_manifest.dedup();

    // Filesystem authority: every crates/<name> directory that carries a Cargo.toml.
    let mut from_disk: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(rust_root().join("crates")) {
        for e in entries.flatten() {
            if e.path().join("Cargo.toml").exists()
                && let Some(name) = e.file_name().to_str()
            {
                from_disk.push(name.to_owned());
            }
        }
    }
    from_disk.sort();
    from_disk.dedup();

    assert_eq!(
        from_manifest,
        from_disk,
        "workspace members drift from on-disk crates: manifest-only={:?} disk-only={:?}",
        from_manifest
            .iter()
            .filter(|c| !from_disk.contains(c))
            .collect::<Vec<_>>(),
        from_disk
            .iter()
            .filter(|c| !from_manifest.contains(c))
            .collect::<Vec<_>>(),
    );
    assert!(
        from_manifest.len() >= 5,
        "workspace scan found only {} crates",
        from_manifest.len()
    );
    from_manifest
}

/// Parse `[workspace].members` from the manifest with a real TOML parser (no hand-written
/// line/bracket/comment dialect), so `members_backup`, a `#` inside a string, a glob, or any
/// formatting variation cannot mislead the scan.
fn workspace_members(manifest: &str) -> Vec<String> {
    let doc: toml::Value = manifest.parse().expect("Cargo.toml is valid TOML");
    let members = doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .expect("[workspace].members array");
    members
        .iter()
        .map(|v| v.as_str().expect("workspace member string").to_owned())
        .collect()
}

#[test]
fn workspace_members_cover_the_on_disk_crates() {
    workspace_crates();
}

#[test]
fn internal_profile_wire_stays_distinct_from_management_wire() {
    let root = rust_root().join("crates/backend-server/src");
    let identity_children: BTreeSet<String> =
        module_declarations(&read(&root.join("identity_access/mod.rs")))
            .into_iter()
            .map(|(name, _)| name)
            .collect();
    assert!(identity_children.contains("agent_wire"));
    assert!(identity_children.contains("agent_profile_wire"));
    assert!(!identity_children.contains("wire"));
}

#[test]
fn local_runtime_catalogue_has_no_remote_coordinate_or_control_plane_detector() {
    use syn::visit::Visit as _;
    let root = rust_root().join("crates/backend-server/src");
    // The coordinate-free local detector face must never import the remote host-control
    // catalogue or a raw host/daemon coordinate. Walk every non-test top-level item, including
    // any production item placed after the test module.
    #[derive(Default)]
    struct LocalCatalogueFacts {
        forbidden: BTreeSet<String>,
    }
    impl<'ast> syn::visit::Visit<'ast> for LocalCatalogueFacts {
        fn visit_path(&mut self, path: &'ast syn::Path) {
            let segments: Vec<String> = path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            if segments
                .windows(2)
                .any(|pair| pair == ["host_runtime", "host_control"])
                || segments.iter().any(|segment| {
                    matches!(
                        segment.as_str(),
                        "ComputerId" | "DaemonInstanceId" | "HostId"
                    )
                })
            {
                self.forbidden.insert(segments.join("::"));
            }
            syn::visit::visit_path(self, path);
        }
    }
    let catalogue = syn::parse_file(&read(&root.join("host_runtime/local_runtime_catalog.rs")))
        .expect("local runtime catalogue parses");
    let mut facts = LocalCatalogueFacts::default();
    for item in &catalogue.items {
        if !item_attrs(item).iter().any(is_cfg_test) {
            facts.visit_item(item);
        }
    }
    assert_eq!(facts.forbidden, BTreeSet::new());
    let control_plane_factory = catalogue
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "control_plane_router" => {
                Some(function)
            }
            _ => None,
        })
        .expect("control_plane_router function");
    assert!(
        control_plane_factory.sig.inputs.is_empty(),
        "the control-plane refusal must have no detector/coordinate dependency"
    );
}

#[test]
fn sqlite_host_agent_ports_share_one_store_authority() {
    let source = rust_root().join("crates/backend-sqlite/src");
    assert!(
        !source.join("host_agent.rs").exists() && !source.join("host_agent").exists(),
        "the split umbrella must be deleted, not retained as a facade"
    );

    let store = source.join("foundation/host_agent_store/mod.rs");
    let store_children: BTreeSet<String> = module_declarations(&read(&store))
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        store_children,
        BTreeSet::from(["decode".to_owned()]),
        "the settled store unit has exactly one internal decode child"
    );
    assert!(
        source
            .join("foundation/host_agent_store/decode.rs")
            .exists()
    );

    fn collect_rs(directory: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).expect("read sqlite source directory") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    let mut sources = Vec::new();
    collect_rs(&source, &mut sources);
    let bodies: Vec<(PathBuf, String)> = sources
        .into_iter()
        .map(|path| {
            let body = read(&path);
            (path, body)
        })
        .collect();
    let containing = |needle: &str| -> Vec<PathBuf> {
        bodies
            .iter()
            .filter(|(_, body)| body.contains(needle))
            .map(|(path, _)| path.clone())
            .collect()
    };
    assert_eq!(
        containing("pub struct SqliteHostAgentStore"),
        vec![store.clone()],
        "the store/pool authority must have one definition"
    );
    assert_eq!(
        containing("impl AgentTeardownReadPort for SqliteHostAgentStore"),
        vec![source.join("application_orchestration/agent_teardown/facts.rs")],
        "application teardown facts own only the application read port"
    );
    assert_eq!(
        containing("impl AgentBulkStopReadPort for SqliteHostAgentStore"),
        vec![source.join("host_runtime/agent_bulk_stop_read.rs")],
        "C2 owns the bulk-stop read port"
    );
    assert_eq!(
        containing("impl AgentProfileMcpPlacementRead for SqliteHostAgentStore"),
        vec![source.join("identity_access/agent_profile_mcp_placement_read.rs")],
        "C1 owns the profile-MCP placement port"
    );
    for runtime in [
        "host_runtime/agent_bulk_stop_read.rs",
        "host_runtime/runtime_switch.rs",
        "host_runtime/runtime_transition.rs",
    ] {
        assert!(
            !read(&source.join(runtime)).contains("crate::application_orchestration"),
            "runtime adapter {runtime} must not depend on the application adapter"
        );
    }
}

#[test]
fn root_compatibility_facades_are_forbidden() {
    let server_src = rust_root().join("crates/backend-server/src");
    let declarations: BTreeSet<String> = module_declarations(&read(&server_src.join("lib.rs")))
        .into_iter()
        .map(|(module, _)| module)
        .collect();
    for module in [
        "agent_materialize",
        "agent_profile_mcp_data_plane",
        "daemon_delivery",
        "runtime_control",
        "scope_runtime_stop",
    ] {
        assert!(
            !server_src.join(format!("{module}.rs")).exists() && !server_src.join(module).exists(),
            "backend-server::{module}: old root facade source returned"
        );
        assert!(
            !declarations.contains(module),
            "backend-server::{module}: old root facade declaration returned"
        );
    }
}
