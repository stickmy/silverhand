//! One authority per closed vocabulary, held over the whole workspace source tree.
//!
//! A closed set has one owner or it has none: the moment a second module declares the
//! same type name, the two can disagree, and the disagreement surfaces as a wire value
//! one half accepts and the other refuses. That is the defect this file exists to keep
//! out — the workspace shipped both a `RuntimeKind` re-listed as a string match in a
//! persistence adapter and two same-named `NotificationId` types on two public paths,
//! and neither was visible to any compiler check.
//!
//! Deliberately source driven rather than type driven: what has to be proven is that a
//! SECOND declaration does not exist, and a type-level test can only speak about the
//! definitions it already names.
//!
//! Read through a real Rust parser, because the property is a property of the
//! declaration grammar and nothing less recognises that grammar. A hand-written scanner
//! answered "no declaration" for `struct r#NotificationId`, whose semantic name IS
//! `NotificationId`, and it could only ever search the item kinds it had been told to
//! search, so `pub type NotificationId = String;` and `pub struct RuntimeKind(String);`
//! were second authorities invisible to it. Every one of those spellings is legal and
//! `cargo fmt --check` clean, and answering each with one more special case just moves
//! the same false green one notch along — a second, permanently incomplete parser.
//!
//! `syn` is therefore a dev-dependency of this crate. It parses text: `#[cfg]` is not
//! evaluated, so a gated duplicate still counts, which is what ownership requires.
//!
//! What a parser cannot see through is macro expansion, and both halves of a macro are
//! unreadable for the same reason: a `macro_rules!` body may bind its own ARGUMENT as a
//! type name, so `declare_type!(NotificationId)` really declares one. Neither a rule
//! body nor an invocation's arguments are therefore read for meaning — if either names a
//! watched type, ownership there is reported as unprovable. That is why this file's own
//! subject, `NotificationId`, is declared and implemented explicitly instead of through
//! the `opaque_id!` shorthand its siblings use.
//!
//! A declaration is not the only path to a name: a public `use` re-exports one, and the
//! duplicate this ticket deleted hid behind exactly such a second public path. Those are
//! inventoried too, while a private `use` — a consumer reaching the one owner — is not.

use std::{
    fs,
    path::{Path, PathBuf},
};

use syn::{Item, ext::IdentExt};

fn rust_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("rust workspace root")
}

/// Every `.rs` file under every crate's `src`, as (workspace-relative path, source).
///
/// `src` and not `tests`: a fixture may legitimately name whatever tokens it drives,
/// while production code may not hold a second copy of the set.
fn production_sources() -> Vec<(String, String)> {
    let root = rust_root();
    let mut sources = Vec::new();
    let crates = fs::read_dir(root.join("crates")).expect("crates directory");
    for entry in crates {
        let source_dir = entry.expect("crate entry").path().join("src");
        if source_dir.is_dir() {
            collect(&root, &source_dir, &mut sources);
        }
    }
    assert!(
        sources.len() > 50,
        "the walk found only {} files, so it is not reading the tree",
        sources.len()
    );
    sources.sort();
    sources
}

fn collect(root: &Path, directory: &Path, sources: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(directory).expect("source directory") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            collect(root, &path, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let relative = path
                .strip_prefix(root)
                .expect("a source under the workspace root")
                .to_string_lossy()
                .into_owned();
            sources.push((relative, fs::read_to_string(&path).expect("source file")));
        }
    }
}

/// Reported in place of an item keyword when the name appears inside token trees the
/// parser hands over unexpanded. Ownership cannot be proven there, and an unprovable
/// declaration must read as a failure, never as an absence.
const UNEXPANDED: &str = "macro tokens";

/// Reported for a source that is not a module at all — an `include!` fragment holding an
/// expression — when the watched name appears in it anyway.
///
/// A fragment that does not parse as items cannot be included at item position either, so
/// it binds no module-level name; the name check is what makes that reasoning observable
/// instead of a skip, and `the_only_sources_that_are_not_modules_are_named` pins which
/// files take this path so a parser that failed on a REAL module cannot hide here.
const UNPARSED: &str = "unparsed fragment";

/// Every module-level declaration of the type name `name` in `source`, as the keyword
/// that declared it.
///
/// The type namespace, not one item kind: `struct`, `enum`, `type`, `union`, `trait` and
/// `trait` aliases all bind a name that a use site can reach, so any of them can be the
/// second authority. Names are compared unrawed, because `r#NotificationId` and
/// `NotificationId` are the same name to the compiler.
///
/// Inline `mod` blocks are walked recursively — `mod foo;` needs no recursion, since the
/// file it names is walked in its own right.
///
/// Two positions are deliberately NOT declarations here: an item inside a function body
/// is reachable by no path, so it cannot sit between two halves of a seam, and an
/// associated type is `<Store as Repo>::Name` rather than a name in a module.
fn declarations_in(source: &str, name: &str) -> Vec<&'static str> {
    let Ok(parsed) = syn::parse_file(source) else {
        return if names(source, name) {
            vec![UNPARSED]
        } else {
            Vec::new()
        };
    };
    let contents = contents_of(&parsed.items);
    let mut declared: Vec<&'static str> = contents
        .declared
        .iter()
        .filter(|(declared, _)| declared == name)
        .map(|(_, kind)| *kind)
        .collect();
    if contents
        .unexpandable
        .iter()
        .any(|tokens| names(tokens, name))
    {
        declared.push(UNEXPANDED);
    }
    declared
}

/// What a module holds, before any question about a particular name is asked.
#[derive(Default)]
struct Contents {
    /// Unrawed type name and the keyword that bound it.
    declared: Vec<(String, &'static str)>,
    /// Token text no parser reads for meaning: `macro_rules!` rules, the arguments of an
    /// invocation, and syntax syn hands back verbatim.
    unexpandable: Vec<String>,
    /// Names of the `macro_rules!` definitions here, whose rules are in `unexpandable`.
    defined: Vec<String>,
    /// Paths of macros invoked at item position, whose expansions this walk cannot see.
    invoked: Vec<String>,
    /// Second public paths to a name, from the `use` items that publish one.
    public_paths: Vec<PublicPath>,
}

/// A path to a type that a public `use` opens, alongside the declaration itself.
#[derive(Clone, Debug, Eq, PartialEq)]
enum PublicPath {
    /// The name this `use` binds — after `as`, when it renames.
    Name(String),
    /// A public glob, rendered with its target path. Which names it carries depends on
    /// another module's public surface, which this reader does not resolve, so a glob is
    /// listed whatever it happens to re-export.
    Glob(String),
}

fn contents_of(items: &[Item]) -> Contents {
    let mut contents = Contents::default();
    walk(items, &mut contents);
    contents
}

fn walk(items: &[Item], contents: &mut Contents) {
    for item in items {
        let (kind, ident) = match item {
            Item::Struct(declaration) => ("struct", &declaration.ident),
            Item::Enum(declaration) => ("enum", &declaration.ident),
            Item::Type(declaration) => ("type", &declaration.ident),
            Item::Union(declaration) => ("union", &declaration.ident),
            Item::Trait(declaration) => ("trait", &declaration.ident),
            Item::TraitAlias(declaration) => ("trait alias", &declaration.ident),
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    walk(nested, contents);
                }
                continue;
            }
            // Neither half of a macro is read for meaning, and for one reason: the rules
            // decide what the arguments become, and a body may bind an argument AS a type
            // name — `declare_type!(NotificationId)` against
            // `($name:ident) => { pub struct $name(String); }` declares that very type.
            // So a DEFINITION contributes its rules and an INVOCATION its arguments, and
            // either naming a watched type makes ownership there unprovable. A definition
            // also records its name, which is how `include!` is separated below from the
            // macros whose rules this tree holds.
            Item::Macro(macro_item) => {
                if let Some(name) = &macro_item.ident {
                    contents.defined.push(name.unraw().to_string());
                } else {
                    contents.invoked.push(path_of(&macro_item.mac.path));
                }
                contents
                    .unexpandable
                    .push(macro_item.mac.tokens.to_string());
                continue;
            }
            Item::Verbatim(tokens) => {
                contents.unexpandable.push(tokens.to_string());
                continue;
            }
            // A `use` with any visibility of its own re-exports what it names, which is a
            // second path to a type declared once — the form the deleted duplicate hid
            // behind. An inherited-visibility `use` is a consumer reaching an owner and
            // says nothing about ownership.
            Item::Use(item) => {
                if !matches!(item.vis, syn::Visibility::Inherited) {
                    use_paths(&item.tree, &mut Vec::new(), &mut contents.public_paths);
                }
                continue;
            }
            _ => continue,
        };
        contents.declared.push((ident.unraw().to_string(), kind));
    }
}

/// Every name a `use` tree binds, and every glob in it, with `prefix` carrying the path
/// segments already traversed so a glob can be reported with its target.
fn use_paths(tree: &syn::UseTree, prefix: &mut Vec<String>, paths: &mut Vec<PublicPath>) {
    match tree {
        syn::UseTree::Path(segment) => {
            prefix.push(segment.ident.unraw().to_string());
            use_paths(&segment.tree, prefix, paths);
            prefix.pop();
        }
        syn::UseTree::Group(group) => {
            for branch in &group.items {
                use_paths(branch, prefix, paths);
            }
        }
        syn::UseTree::Name(leaf) => paths.push(PublicPath::Name(leaf.ident.unraw().to_string())),
        // The bound name is the one after `as`: renaming a watched type still publishes it
        // under whatever the alias says, so the alias is what an inventory has to carry.
        syn::UseTree::Rename(leaf) => paths.push(PublicPath::Name(leaf.rename.unraw().to_string())),
        syn::UseTree::Glob(_) => paths.push(PublicPath::Glob(format!("{}::*", prefix.join("::")))),
    }
}

fn path_of(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// Whether `tokens` holds `name` as a whole identifier.
///
/// Only ever applied to unexpanded token trees, where the conservative direction is to
/// over-report: a macro that merely mentions the name fails this gate and has to be
/// answered, which is the opposite of a declaration hiding inside one. Token text, so a
/// raw identifier arrives as `r#Name` and the `#` ends the identifier before it.
fn names(tokens: &str, name: &str) -> bool {
    tokens.match_indices(name).any(|(at, _)| {
        !tokens[..at]
            .chars()
            .next_back()
            .is_some_and(continues_identifier)
            && !tokens[at + name.len()..]
                .chars()
                .next()
                .is_some_and(continues_identifier)
    })
}

fn continues_identifier(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Whether `file` declares the child module `name` with a visibility of its own.
///
/// A missing declaration panics rather than answering "not public": a module that moved or
/// was renamed must be a red gate, not a premise that quietly holds.
fn declares_public_module(file: &str, name: &str) -> bool {
    let source = fs::read_to_string(rust_root().join(file)).expect("a module file");
    let parsed = syn::parse_file(&source).expect("a module");
    let declaration = parsed
        .items
        .iter()
        .find_map(|item| match item {
            Item::Mod(module) if module.ident.unraw() == name => Some(&module.vis),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{file} declares no module {name}"));
    !matches!(declaration, syn::Visibility::Inherited)
}

/// The public paths a source opens, as the re-export gate reads them.
///
/// A source that does not parse as a module holds an expression, so it binds no `use` at
/// all; that it does not parse is what `declarations_in` reports, and which files those
/// are is pinned by `the_only_sources_that_are_not_modules_are_named`.
fn public_paths_in(source: &str) -> Vec<PublicPath> {
    match syn::parse_file(source) {
        Ok(parsed) => contents_of(&parsed.items).public_paths,
        Err(_) => Vec::new(),
    }
}

/// Where the type name `name` is declared across the production tree, as
/// (file, item keyword) so a failure names both the duplicate and the kind it took.
fn declarations_of(name: &str) -> Vec<(String, &'static str)> {
    production_sources()
        .into_iter()
        .flat_map(|(path, source)| {
            declarations_in(&source, name)
                .into_iter()
                .map(move |kind| (path.clone(), kind))
        })
        .collect()
}

/// The original six-token signature of a duplicate runtime authority.
/// This conservative guard also catches old copies after new runtimes are added;
/// current wire membership is owned by RuntimeKind and its cross-language tests.
const RUNTIME_TOKENS: [&str; 6] = ["claude", "codex", "cursor", "gemini", "qoder", "opencode"];

/// Whether this source knows the WHOLE runtime set — the shape a second authority
/// takes, whether it is written as a match, an array or a list of constants.
///
/// Naming one or two tokens is not that shape: a decode test and a seeded fixture
/// both do it legitimately, and forbidding it would say nothing about ownership.
///
/// Raw text, unlike the declaration reader: this predicate is ABOUT quoted literals, so
/// the tree it wants is the one the file was written with. A prose list of all six in a
/// comment would be reported, which is the harmless direction — a name to answer, not a
/// second authority admitted.
fn names_the_whole_runtime_set(source: &str) -> bool {
    RUNTIME_TOKENS
        .iter()
        .all(|token| source.contains(&format!("\"{token}\"")))
}

#[test]
fn the_runtime_closed_set_has_exactly_one_owner() {
    // Control, both directions, so a predicate that answered "no" to everything
    // cannot pass this gate silently.
    assert!(names_the_whole_runtime_set(
        "match raw { \"claude\" | \"codex\" | \"cursor\" | \"gemini\" | \"qoder\" | \"opencode\" => () }"
    ));
    assert!(!names_the_whole_runtime_set(
        "\"claude\" and \"codex\" only"
    ));

    const OWNER: &str = "crates/backend-core/src/identity_access/agents/runtime.rs";
    assert_eq!(
        declarations_of("RuntimeKind"),
        vec![(OWNER.to_owned(), "enum")],
        "the runtime closed set must be declared exactly once, as the enum in the agents \
         facade: any other declaration of that name is a second domain for the same column"
    );

    let second_authorities: Vec<String> = production_sources()
        .into_iter()
        .filter(|(path, source)| path != OWNER && names_the_whole_runtime_set(source))
        .map(|(path, _)| path)
        .collect();
    assert!(
        second_authorities.is_empty(),
        "{second_authorities:?} restate the whole runtime set: a closed vocabulary spelled \
         out twice can disagree with its owner, and the disagreement reaches the wire"
    );
}

/// Two same-named identity types are worse than two different ones: each half stays
/// internally consistent, so nothing fails, and the seam between them is paid for with
/// a `String` round trip that accepts values neither half would mint.
///
/// The reader's own controls are one test down.
#[test]
fn the_notification_identity_has_exactly_one_declaration() {
    assert_eq!(
        declarations_of("NotificationId"),
        vec![(
            "crates/backend-core/src/conversation_messaging/messaging/identity.rs".to_owned(),
            "struct"
        )],
        "the notification identity must be declared once, where it is minted"
    );
}

/// The other shape of the same defect: one closed refusal set declared in the adapter crate
/// AND in the route crate, with the composition root translating between them. Two
/// one-variant enums cannot disagree today, which is exactly the trap — the translation is
/// where a later variant gets dropped, and nothing in the tree would say so.
#[test]
fn the_stale_resolution_refusal_has_exactly_one_declaration() {
    assert_eq!(
        declarations_of("StaleMessageResolutionAdmissionRefusal"),
        vec![(
            "crates/backend-core/src/conversation_messaging/messaging/collaboration/stale_message_resolution.rs"
                .to_owned(),
            "enum"
        )],
        "the stale-resolution refusal must be declared once, where the admission contract \
         lives: a copy in an adapter or a route crate needs a mapping, and a mapping is what \
         loses a variant silently"
    );
}

/// The other way a path to a name appears: a public `use`.
///
/// The duplicate this ticket deleted was reachable as `collaboration::notification::model::
/// NotificationId` as well as where it was defined, and deleting a definition while leaving
/// the re-export would let the split recur with nothing in the tree to notice — the
/// declaration inventory reads items, and a `use` declares nothing.
///
/// This reader does not resolve module visibility, so it lists every public path and each
/// one has to be named here. That is what separates the two shapes the tree actually uses:
/// `NotificationId` is declared in a PRIVATE module and published by the messaging facade,
/// so that re-export is its only public path, while `RuntimeKind` is declared in a public
/// module and re-exported nowhere. Either way one path, and any addition is red.
///
/// Globs are listed whatever they carry: which names `pub use read::*` publishes depends
/// on another module's public surface, so pinning the set is what keeps `pub use
/// identity::*;` from becoming a silent second path. The cost is that a new public glob
/// anywhere reddens this gate and has to be answered.
///
/// A private `use` is untouched — every consumer of these types has one, and reaching an
/// owner is the opposite of being one.
#[test]
fn no_public_use_opens_a_second_path_to_a_watched_name() {
    // The premise the inventory below is read against, as one pair so a single run reports
    // both: the identity is publishable ONLY through the facade, and the runtime set only
    // at its declaration. Turning `mod identity` public would give the identity a second
    // public path while every entry below stayed exactly as it is.
    assert_eq!(
        (
            declares_public_module(
                "crates/backend-core/src/conversation_messaging/messaging/mod.rs",
                "identity"
            ),
            declares_public_module(
                "crates/backend-core/src/identity_access/agents/mod.rs",
                "runtime"
            ),
        ),
        (false, true),
        "the identity's module is private and the runtime set's module is public, which is \
         why exactly one of the two vocabularies is re-exported by name"
    );

    let mut second_paths: Vec<(String, PublicPath)> = Vec::new();
    for (file, source) in production_sources() {
        for path in public_paths_in(&source) {
            let second = match &path {
                PublicPath::Name(name) => WATCHED.contains(&name.as_str()),
                PublicPath::Glob(_) => true,
            };
            if second {
                second_paths.push((file.clone(), path));
            }
        }
    }

    let attachment =
        "crates/backend-core/src/conversation_messaging/messaging/collaboration/attachment/mod.rs";
    let directory = "crates/backend-core/src/conversation_messaging/messaging/collaboration/channel_directory/mod.rs";
    let lifecycle = "crates/backend-core/src/conversation_messaging/messaging/collaboration/channel_lifecycle.rs";
    assert_eq!(
        second_paths,
        vec![
            (
                attachment.to_owned(),
                PublicPath::Glob("read::*".to_owned())
            ),
            (
                attachment.to_owned(),
                PublicPath::Glob("write::*".to_owned())
            ),
            (
                directory.to_owned(),
                PublicPath::Glob("create::*".to_owned())
            ),
            (directory.to_owned(), PublicPath::Glob("read::*".to_owned())),
            (
                lifecycle.to_owned(),
                PublicPath::Glob("mutation::*".to_owned())
            ),
            (
                "crates/backend-core/src/conversation_messaging/messaging/collaboration/human_search.rs".to_owned(),
                PublicPath::Glob("identities::*".to_owned())
            ),
            (
                "crates/backend-core/src/conversation_messaging/messaging/mod.rs".to_owned(),
                PublicPath::Name("NotificationId".to_owned())
            ),
        ],
        "each public path to a watched name has to be one of these: the messaging facade \
         publishing the private identity module, and reviewed local globs; none of \
         their targets publicly exports a watched name"
    );
}

/// What the re-export reader must report, and what it must leave alone. Without both
/// directions a predicate that answered "nothing" everywhere would pass the gate above
/// while every second path in the tree stayed invisible.
#[test]
fn the_reexport_reader_separates_publishing_from_consuming() {
    let watched = |name: &str| PublicPath::Name(name.to_owned());
    let cases: [(&str, Vec<PublicPath>, &str); 8] = [
        (
            "pub use crate::messaging::identity::NotificationId;",
            vec![watched("NotificationId")],
            "the form the deleted duplicate hid behind: a second public path to the one \
             declaration",
        ),
        (
            "use crate::messaging::identity::NotificationId;",
            Vec::new(),
            "a private import is a consumer reaching the owner, which is what every \
             consumer of these types does",
        ),
        (
            "pub(crate) use crate::messaging::identity::NotificationId;",
            vec![watched("NotificationId")],
            "a crate-visible re-export still publishes the name to every module that \
             could hold the second half of a seam",
        ),
        (
            "pub(in crate::messaging) use identity::NotificationId;",
            vec![watched("NotificationId")],
            "and so does a restricted one: any visibility of its own makes it a path",
        ),
        (
            "pub use identity::MessageId as NotificationId;",
            vec![watched("NotificationId")],
            "an alias publishes the name it binds, not the one it came from, so the \
             alias is what the inventory carries",
        ),
        (
            "pub use identity::{MessageId, NotificationId};",
            vec![watched("MessageId"), watched("NotificationId")],
            "a group binds each of its leaves",
        ),
        (
            "pub use identity::*;",
            vec![PublicPath::Glob("identity::*".to_owned())],
            "a glob spells no name at all, so it is reported by target: whether it \
             carries a watched type depends on a module this reader does not resolve",
        ),
        (
            "pub mod inner {\n    pub use crate::messaging::identity::NotificationId;\n}",
            vec![watched("NotificationId")],
            "inline modules are walked, or a re-export hides one brace deep",
        ),
    ];

    for (source, expected, reason) in cases {
        assert_eq!(public_paths_in(source), expected, "{reason}\n{source}");
    }
}

/// What the reader must recognise as a declaration of a watched name, and what it must
/// not. An inventory is only as good as its reader: one blind to a legal spelling
/// reports "exactly one declaration" while a second one sits in the tree, and the
/// mutation that should have caught it passes.
///
/// Every row expecting a declaration is a spelling rustfmt leaves alone, so each is a
/// way for a second authority to enter a tree that is `cargo fmt --check` clean. The
/// rows carrying the property are the ones a text scanner cannot answer: a raw
/// identifier, whose semantic name has no `r#` in it, and the item kinds other than the
/// one a search string happens to name.
#[test]
fn the_declaration_reader_follows_rust_declaration_grammar() {
    let cases: [(&str, &str, &[&str], &str); 22] = [
        (
            "pub struct NotificationId(String);",
            "NotificationId",
            &["struct"],
            "the plain declaration",
        ),
        (
            "pub struct r#NotificationId(String);",
            "NotificationId",
            &["struct"],
            "a raw identifier declares the unrawed name: to the compiler this IS \
             NotificationId, while text carrying `r#` matches no search string",
        ),
        (
            "pub enum r#RuntimeKind {\n    Claude,\n}",
            "RuntimeKind",
            &["enum"],
            "the same for the runtime set",
        ),
        (
            "pub type NotificationId = String;",
            "NotificationId",
            &["type"],
            "an alias binds the same name in the same namespace, so it is a second \
             authority even though no `struct` keyword appears",
        ),
        (
            "pub struct RuntimeKind(String);",
            "RuntimeKind",
            &["struct"],
            "and a struct can restate a name the owner holds as an enum, which is \
             exactly how a closed set is reopened",
        ),
        (
            "pub union RuntimeKind {\n    tag: u8,\n}",
            "RuntimeKind",
            &["union"],
            "unions are in the type namespace too",
        ),
        (
            "pub trait NotificationId {}",
            "NotificationId",
            &["trait"],
            "so are traits",
        ),
        (
            "pub mod inner {\n    pub mod deeper {\n        pub struct NotificationId(String);\n    }\n}",
            "NotificationId",
            &["struct"],
            "inline modules are walked recursively, or a duplicate hides one brace deep",
        ),
        (
            "pub /* gap */ struct NotificationId(String);",
            "NotificationId",
            &["struct"],
            "trivia between visibility and item is not the parser's problem at all",
        ),
        (
            "#[cfg(test)]\npub struct NotificationId(String);",
            "NotificationId",
            &["struct"],
            "cfg is not evaluated, and must not be: a gated duplicate can still \
             disagree with the owner",
        ),
        (
            "macro_rules! shorthand {\n    () => {\n        pub struct NotificationId(String);\n    };\n}",
            "NotificationId",
            &[UNEXPANDED],
            "no parser sees through expansion, so a macro naming a watched type is \
             reported as unprovable rather than silently absent",
        ),
        (
            "macro_rules! declare_type {\n    ($name:ident) => {\n        pub struct $name(String);\n    };\n}",
            "NotificationId",
            &[],
            "and the definition side alone CANNOT answer this one: its rules hold only \
             `$name`, so no watched name appears anywhere in them",
        ),
        (
            "pub mod hidden {\n    declare_type!(NotificationId);\n}",
            "NotificationId",
            &[UNEXPANDED],
            "which is why an invocation's arguments are unreadable too: this pair really \
             declares hidden::NotificationId, and reading the invocation as \"only \
             arguments\" admitted a second owner in a fmt-clean tree",
        ),
        (
            "opaque_id!(NotificationId);",
            "NotificationId",
            &[UNEXPANDED],
            "so the owner's own shorthand is reported as well — no argument naming a \
             watched type is provable, which is why that one invocation is written out \
             explicitly instead of exempted by name",
        ),
        (
            "opaque_id!(MessageId);",
            "NotificationId",
            &[],
            "while a macro whose arguments name nothing watched stays green: the test is \
             the arguments, not the presence of a macro",
        ),
        (
            "pub struct NotificationIdentity(String);",
            "NotificationId",
            &[],
            "a longer name is a different name: identifiers are compared, not substrings",
        ),
        (
            "/* outer /* inner */ pub struct NotificationId(String); */",
            "NotificationId",
            &[],
            "nested comments close by depth in the grammar, so the tail of an outer \
             comment is not code",
        ),
        (
            "const SAMPLE: &str = r#\"pub struct NotificationId(String);\"#;",
            "NotificationId",
            &[],
            "a raw string literal declares nothing, whatever its body spells",
        ),
        (
            "fn build() {\n    pub struct NotificationId(String);\n}",
            "NotificationId",
            &[],
            "a body-local item is reachable by no path, so it cannot be the far side \
             of a seam",
        ),
        (
            "impl Repo for Store {\n    type NotificationId = String;\n}",
            "NotificationId",
            &[],
            "an associated type is <Store as Repo>::NotificationId, not a name bound \
             in a module",
        ),
        (
            "&[Case {\n    name: \"once_terminal\",\n}]",
            "NotificationId",
            &[],
            "an include! fragment holds an expression, not items, and this one names \
             nothing watched",
        ),
        (
            "&[Case {\n    name: \"NotificationId\",\n}]",
            "NotificationId",
            &[UNPARSED],
            "the same fragment naming a watched type is reported rather than skipped: \
             a source the reader cannot parse must never read as an absence",
        ),
    ];

    for (source, name, expected, reason) in cases {
        assert_eq!(
            declarations_in(source, name),
            expected,
            "{reason}\n{source}"
        );
    }
}

/// Which production sources the reader cannot parse as modules, pinned by name.
///
/// A parse failure is handled conservatively one function up, but "conservatively" is
/// only true while the failures are the three generated `include!` fragments: if a real
/// module ever stopped parsing — new syntax, an older parser — it would take the same
/// path with far more to hide. Pinning the set turns that into a red gate and a name to
/// answer instead of a silent narrowing.
#[test]
fn the_only_sources_that_are_not_modules_are_named() {
    let fragments: Vec<String> = production_sources()
        .into_iter()
        .filter(|(_, source)| syn::parse_file(source).is_err())
        .map(|(path, _)| path)
        .collect();
    assert_eq!(
        fragments,
        vec![
            "crates/backend-core/src/conversation_messaging/messaging/schedule/recurrence_ts_fixture.rs".to_owned(),
            "crates/backend-core/src/conversation_messaging/messaging/schedule/settlement_ts_fixture.rs".to_owned(),
            "crates/backend-core/src/conversation_messaging/messaging/task/event_ts_fixture.rs".to_owned(),
        ],
        "these sources are read by identifier rather than by declaration, so the set \
         has to stay the three TS-signed expression fixtures"
    );
}

/// The names this file holds to one owner each.
const WATCHED: [&str; 3] = [
    "NotificationId",
    "RuntimeKind",
    "StaleMessageResolutionAdmissionRefusal",
];

/// Nothing at item position may bind a type name this inventory cannot read.
///
/// Expansion is the one thing a parser cannot follow, so each item-position macro has to
/// be answered. A macro whose `macro_rules!` lives in this tree is answered already: its
/// rules are scanned as unexpandable tokens wherever they are defined. `include!` is the
/// exception, and it has two targets — a path under `src`, which this walk reads as a
/// source in its own right, and build-script output, which is generated rather than
/// written. The second target is closed here rather than described: a build script that
/// never names a watched type cannot emit a declaration of one.
#[test]
fn nothing_at_item_position_hides_declarations_from_this_inventory() {
    let mut invoked = Vec::new();
    let mut defined = Vec::new();
    for (_, source) in production_sources() {
        if let Ok(parsed) = syn::parse_file(&source) {
            let contents = contents_of(&parsed.items);
            invoked.extend(contents.invoked);
            defined.extend(contents.defined);
        }
    }
    let mut unreadable: Vec<String> = invoked
        .into_iter()
        .filter(|invocation| !defined.contains(invocation))
        .collect();
    unreadable.sort();
    unreadable.dedup();
    assert_eq!(
        unreadable,
        vec!["include".to_owned()],
        "a macro invoked at item position whose rules are not in this tree can expand \
         into a declaration nothing here reads"
    );

    let generators: Vec<String> = fs::read_dir(rust_root().join("crates"))
        .expect("crates directory")
        .filter_map(|entry| {
            let script = entry.expect("crate entry").path().join("build.rs");
            let source = fs::read_to_string(&script).ok()?;
            WATCHED
                .iter()
                .any(|name| names(&source, name))
                .then(|| script.to_string_lossy().into_owned())
        })
        .collect();
    assert!(
        generators.is_empty(),
        "{generators:?} name a watched type, so what they generate into OUT_DIR and \
         include! at item position could declare it out of this inventory's reach"
    );
}
