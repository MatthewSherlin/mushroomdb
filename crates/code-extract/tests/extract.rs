//! Behaviour tests for structure extraction and reference resolution.
//!
//! The fixtures under `tests/fixtures` are synthetic files written for these
//! tests. Resolution, on the other hand, is tested against *path sets* rather
//! than a directory tree: the crate never touches the filesystem, so the
//! working tree it resolves against is whatever the `known` and `files_in`
//! closures say it is. Building that set in the test is both closer to how the
//! caller uses the crate and immune to anything cargo does to a fixture
//! directory.

use code_extract::{
    extract, lang_of, resolve_call, resolve_import, resolve_mention, FileFacts, Lang, SymbolIndex,
    MAX_BODY_BYTES, MAX_FILE_BYTES,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// ── helpers ─────────────────────────────────────────────────────────────────

fn fixture_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

fn read_fixture(rel: &str) -> Vec<u8> {
    std::fs::read(fixture_path(rel)).unwrap_or_else(|e| panic!("fixture {rel}: {e}"))
}

/// Extract a fixture, pretending it lives at `as_path` in a working tree.
fn facts(fixture: &str, as_path: &str) -> FileFacts {
    extract(as_path, &read_fixture(fixture))
}

fn tree(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| (*p).to_string()).collect()
}

/// Directory enumeration over a path set: the files directly inside `dir`.
fn files_under(files: &BTreeSet<String>, dir: &str) -> Vec<String> {
    files
        .iter()
        .filter(|path| {
            let parent = path.rsplit_once('/').map_or("", |(head, _)| head);
            parent == dir
        })
        .cloned()
        .collect()
}

/// Every language but Go resolves through `known` alone; this stands in for
/// the enumeration they never ask for.
fn no_files(_dir: &str) -> Vec<String> {
    Vec::new()
}

fn names(facts: &FileFacts) -> Vec<&str> {
    facts.symbols.iter().map(|s| s.name.as_str()).collect()
}

fn kind_of<'a>(facts: &'a FileFacts, name: &str) -> &'a str {
    facts
        .symbols
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no symbol {name} in {:?}", names(facts)))
        .kind
}

fn doc_of<'a>(facts: &'a FileFacts, name: &str) -> &'a str {
    facts
        .symbols
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no symbol {name} in {:?}", names(facts)))
        .doc
        .as_str()
}

fn callees<'a>(facts: &'a FileFacts, name: &str) -> Vec<&'a str> {
    facts
        .symbols
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no symbol {name} in {:?}", names(facts)))
        .calls
        .iter()
        .map(|(callee, _)| callee.as_str())
        .collect()
}

fn raw_imports(facts: &FileFacts) -> Vec<&str> {
    facts.imports.iter().map(|i| i.raw.as_str()).collect()
}

/// Independent naive check that every call landed on the innermost symbol
/// containing it: for each recorded call, no other symbol with a strictly
/// narrower line range may also contain that line. Quadratic on purpose —
/// this is the scan the library deliberately does not do.
fn assert_calls_are_innermost(facts: &FileFacts, label: &str) {
    for owner in &facts.symbols {
        let owner_span = owner.line_end - owner.line_start;
        for (callee, line) in &owner.calls {
            for other in &facts.symbols {
                if other.name == owner.name && other.line_start == owner.line_start {
                    continue;
                }
                let contains = other.line_start <= *line && *line <= other.line_end;
                let narrower = other.line_end - other.line_start < owner_span;
                assert!(
                    !(contains && narrower),
                    "{label}: call to {callee} on line {line} landed on {} \
                     but {} is narrower and also contains it",
                    owner.name,
                    other.name
                );
            }
        }
    }
}

// ── Rust resolution ─────────────────────────────────────────────────────────

#[test]
fn rust_module_declarations_resolve_to_file_or_mod_rs() {
    let files = tree(&[
        "crates/alpha/src/lib.rs",
        "crates/alpha/src/util.rs",
        "crates/alpha/src/net/mod.rs",
        "crates/alpha/src/net/client.rs",
    ]);
    let known = |p: &str| files.contains(p);

    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/lib.rs",
            "mod util",
            &known,
            &no_files
        ),
        vec!["crates/alpha/src/util.rs"]
    );
    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/lib.rs",
            "mod net",
            &known,
            &no_files
        ),
        vec!["crates/alpha/src/net/mod.rs"]
    );
    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/net/mod.rs",
            "mod client",
            &known,
            &no_files
        ),
        vec!["crates/alpha/src/net/client.rs"]
    );
    assert!(resolve_import(
        Lang::Rust,
        "crates/alpha/src/lib.rs",
        "mod absent",
        &known,
        &no_files
    )
    .is_empty());
}

#[test]
fn rust_crate_paths_resolve_under_the_nearest_manifest() {
    let files = tree(&[
        "Cargo.toml",
        "crates/alpha/Cargo.toml",
        "crates/alpha/src/lib.rs",
        "crates/alpha/src/util.rs",
        "crates/alpha/src/net/mod.rs",
        "crates/alpha/src/net/client.rs",
    ]);
    let known = |p: &str| files.contains(p);

    // `helper` is an item inside `util`, so the module file is the target.
    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/net/client.rs",
            "crate::util::helper",
            &known,
            &no_files
        ),
        vec!["crates/alpha/src/util.rs"]
    );
    // The longer module path wins when it exists.
    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/lib.rs",
            "crate::net::client",
            &known,
            &no_files
        ),
        vec!["crates/alpha/src/net/client.rs"]
    );
    // A `crate::` path in a file with no manifest above it resolves to nothing.
    assert!(resolve_import(
        Lang::Rust,
        "scratch/loose.rs",
        "crate::util",
        &known,
        &no_files
    )
    .is_empty());
}

#[test]
fn rust_workspace_package_paths_resolve_to_lib_rs() {
    let files = tree(&[
        "Cargo.toml",
        "crates/alpha/Cargo.toml",
        "crates/alpha/src/lib.rs",
        "crates/beta-core/Cargo.toml",
        "crates/beta-core/src/lib.rs",
    ]);
    let known = |p: &str| files.contains(p);

    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/lib.rs",
            "beta_core::Ledger",
            &known,
            &no_files
        ),
        vec!["crates/beta-core/src/lib.rs"]
    );

    // No layout convention is assumed: a workspace that keeps its members
    // somewhere other than `crates/` resolves the same way.
    let libs = tree(&[
        "Cargo.toml",
        "libs/alpha/Cargo.toml",
        "libs/alpha/src/lib.rs",
        "libs/gamma-util/Cargo.toml",
        "libs/gamma-util/src/lib.rs",
        // A directory with a `src/lib.rs` but no manifest is not a package.
        "vendored/delta/src/lib.rs",
    ]);
    let known = |p: &str| libs.contains(p);
    assert_eq!(
        resolve_import(
            Lang::Rust,
            "libs/alpha/src/lib.rs",
            "gamma_util::Thing",
            &known,
            &no_files
        ),
        vec!["libs/gamma-util/src/lib.rs"]
    );
    assert!(resolve_import(
        Lang::Rust,
        "libs/alpha/src/lib.rs",
        "delta::Thing",
        &known,
        &no_files
    )
    .is_empty());
}

#[test]
fn rust_self_paths_resolve_within_the_module_not_a_sibling_package() {
    // `inner` names both a child module of `net` and a workspace package.
    // `self::inner` must mean the module.
    let files = tree(&[
        "Cargo.toml",
        "crates/alpha/Cargo.toml",
        "crates/alpha/src/lib.rs",
        "crates/alpha/src/util.rs",
        "crates/alpha/src/net/mod.rs",
        "crates/alpha/src/net/inner.rs",
        "crates/inner/Cargo.toml",
        "crates/inner/src/lib.rs",
    ]);
    let known = |p: &str| files.contains(p);

    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/net/mod.rs",
            "self::inner::Thing",
            &known,
            &no_files
        ),
        vec!["crates/alpha/src/net/inner.rs"]
    );
    // Without `self::`, the same first segment is a package name, which is
    // exactly the wrong edge `self::` has to avoid.
    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/net/mod.rs",
            "inner::Thing",
            &known,
            &no_files
        ),
        vec!["crates/inner/src/lib.rs"]
    );
    // A non-`mod.rs` file owns a directory named after its stem.
    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/net.rs",
            "self::inner",
            &known,
            &no_files
        ),
        vec!["crates/alpha/src/net/inner.rs"]
    );
    // A trailing `self` (`use crate::util::{self}`) names the module the
    // preceding segments already name.
    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/lib.rs",
            "crate::util::self",
            &known,
            &no_files
        ),
        vec!["crates/alpha/src/util.rs"]
    );
}

#[test]
fn rust_super_paths_resolve_relative_to_the_module() {
    let files = tree(&[
        "crates/alpha/Cargo.toml",
        "crates/alpha/src/lib.rs",
        "crates/alpha/src/util.rs",
        "crates/alpha/src/net/mod.rs",
        "crates/alpha/src/net/client.rs",
        "crates/alpha/src/net/server.rs",
    ]);
    let known = |p: &str| files.contains(p);

    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/net/server.rs",
            "super::client::connect",
            &known,
            &no_files
        ),
        vec!["crates/alpha/src/net/client.rs"]
    );
    assert_eq!(
        resolve_import(
            Lang::Rust,
            "crates/alpha/src/net/client.rs",
            "super::super::util::helper",
            &known,
            &no_files
        ),
        vec!["crates/alpha/src/util.rs"]
    );
}

#[test]
fn rust_external_crates_resolve_to_nothing() {
    let files = tree(&[
        "Cargo.toml",
        "crates/alpha/Cargo.toml",
        "crates/alpha/src/lib.rs",
    ]);
    let known = |p: &str| files.contains(p);

    for raw in ["std::io::Read", "serde::Serialize", "core::fmt"] {
        assert!(
            resolve_import(
                Lang::Rust,
                "crates/alpha/src/lib.rs",
                raw,
                &known,
                &no_files
            )
            .is_empty(),
            "{raw} should not resolve"
        );
    }
}

// ── Python resolution ───────────────────────────────────────────────────────

#[test]
fn python_dotted_modules_resolve_to_files_and_packages() {
    let files = tree(&[
        "app/service.py",
        "app/sibling.py",
        "pkg/__init__.py",
        "pkg/mod_a.py",
        "pkg/sub/__init__.py",
        "pkg/sub/deep.py",
    ]);
    let known = |p: &str| files.contains(p);

    assert_eq!(
        resolve_import(
            Lang::Python,
            "app/service.py",
            "pkg.mod_a",
            &known,
            &no_files
        ),
        vec!["pkg/mod_a.py"]
    );
    assert_eq!(
        resolve_import(Lang::Python, "app/service.py", "pkg.sub", &known, &no_files),
        vec!["pkg/sub/__init__.py"]
    );
    assert_eq!(
        resolve_import(
            Lang::Python,
            "app/service.py",
            "pkg.sub.deep",
            &known,
            &no_files
        ),
        vec!["pkg/sub/deep.py"]
    );
}

#[test]
fn python_relative_imports_walk_upwards() {
    let files = tree(&["app/service.py", "app/sibling.py", "shared/util.py"]);
    let known = |p: &str| files.contains(p);

    assert_eq!(
        resolve_import(
            Lang::Python,
            "app/service.py",
            ".sibling",
            &known,
            &no_files
        ),
        vec!["app/sibling.py"]
    );
    assert_eq!(
        resolve_import(
            Lang::Python,
            "app/service.py",
            "..shared.util",
            &known,
            &no_files
        ),
        vec!["shared/util.py"]
    );
}

#[test]
fn python_stdlib_modules_resolve_to_nothing() {
    // `os.py` exists in the tree and is still not what `import os` means.
    let files = tree(&["app/service.py", "os.py", "json/__init__.py"]);
    let known = |p: &str| files.contains(p);

    assert!(resolve_import(Lang::Python, "app/service.py", "os", &known, &no_files).is_empty());
    assert!(
        resolve_import(Lang::Python, "app/service.py", "os.path", &known, &no_files).is_empty()
    );
    assert!(resolve_import(Lang::Python, "app/service.py", "json", &known, &no_files).is_empty());
}

// ── TypeScript and JavaScript resolution ────────────────────────────────────

#[test]
fn typescript_specifiers_try_extensions_and_index_files() {
    let files = tree(&[
        "src/index.ts",
        "src/util.ts",
        "src/types.ts",
        "src/legacy.ts",
        "src/widget/index.tsx",
    ]);
    let known = |p: &str| files.contains(p);

    assert_eq!(
        resolve_import(
            Lang::TypeScript,
            "src/index.ts",
            "./util",
            &known,
            &no_files
        ),
        vec!["src/util.ts"]
    );
    assert_eq!(
        resolve_import(
            Lang::TypeScript,
            "src/index.ts",
            "./widget",
            &known,
            &no_files
        ),
        vec!["src/widget/index.tsx"]
    );
    // `export … from "./types"` resolves like any other specifier.
    assert_eq!(
        resolve_import(
            Lang::TypeScript,
            "src/index.ts",
            "./types",
            &known,
            &no_files
        ),
        vec!["src/types.ts"]
    );
    assert!(resolve_import(
        Lang::TypeScript,
        "src/index.ts",
        "node:path",
        &known,
        &no_files
    )
    .is_empty());
    assert!(resolve_import(
        Lang::TypeScript,
        "src/index.ts",
        "somelib",
        &known,
        &no_files
    )
    .is_empty());
}

#[test]
fn typescript_js_specifiers_fall_back_to_ts_sources() {
    let files = tree(&["src/index.ts", "src/legacy.ts", "src/widget/index.tsx"]);
    let known = |p: &str| files.contains(p);

    assert_eq!(
        resolve_import(
            Lang::TypeScript,
            "src/index.ts",
            "./legacy.js",
            &known,
            &no_files
        ),
        vec!["src/legacy.ts"]
    );
    assert_eq!(
        resolve_import(
            Lang::Tsx,
            "src/widget/index.tsx",
            "../legacy.js",
            &known,
            &no_files
        ),
        vec!["src/legacy.ts"]
    );
}

#[test]
fn javascript_relative_specifiers_resolve() {
    let files = tree(&["src/main.js", "src/loader.mjs", "src/parser.js"]);
    let known = |p: &str| files.contains(p);

    assert_eq!(
        resolve_import(
            Lang::JavaScript,
            "src/main.js",
            "./loader.mjs",
            &known,
            &no_files
        ),
        vec!["src/loader.mjs"]
    );
    assert_eq!(
        resolve_import(
            Lang::JavaScript,
            "src/main.js",
            "./parser",
            &known,
            &no_files
        ),
        vec!["src/parser.js"]
    );
}

// ── Go resolution ───────────────────────────────────────────────────────────

#[test]
fn go_imports_resolve_to_every_file_in_the_package() {
    // A Go import names a package, which is a directory, so one import
    // reaches every non-test source in it.
    let files = tree(&[
        "cmd/app/main.go",
        "store/store.go",
        "store/index.go",
        "store/store_test.go",
        "store/README.md",
        "store/deep/deep.go",
        "docs/store/notes.md",
    ]);
    let known = |p: &str| files.contains(p);
    let files_in = |dir: &str| files_under(&files, dir);

    assert_eq!(
        resolve_import(
            Lang::Go,
            "cmd/app/main.go",
            "example.com/demo/store",
            &known,
            &files_in
        ),
        vec!["store/index.go", "store/store.go"]
    );
    // A nested package resolves on its own, not through its parent.
    assert_eq!(
        resolve_import(
            Lang::Go,
            "cmd/app/main.go",
            "example.com/demo/store/deep",
            &known,
            &files_in
        ),
        vec!["store/deep/deep.go"]
    );
    // A directory that matches by name but holds no Go sources is not the
    // package, and the search continues to one that is.
    let prose = tree(&[
        "cmd/app/main.go",
        "cmd/app/notes/notes.go",
        "docs/notes/notes.md",
        "notes/README.md",
    ]);
    assert_eq!(
        resolve_import(
            Lang::Go,
            "cmd/app/main.go",
            "example.com/demo/notes",
            &|p: &str| prose.contains(p),
            &|dir: &str| files_under(&prose, dir)
        ),
        vec!["cmd/app/notes/notes.go"]
    );
    assert!(resolve_import(Lang::Go, "cmd/app/main.go", "fmt", &known, &files_in).is_empty());
    assert!(resolve_import(
        Lang::Go,
        "cmd/app/main.go",
        "golang.org/x/sync",
        &known,
        &files_in
    )
    .is_empty());
    // Nothing this crate returns is ever a directory.
    for path in resolve_import(
        Lang::Go,
        "cmd/app/main.go",
        "example.com/demo/store",
        &known,
        &files_in,
    ) {
        assert!(!path.ends_with('/'), "{path} is a directory");
        assert!(known(&path), "{path} is not a file in the tree");
    }
}

// ── Markdown mentions ───────────────────────────────────────────────────────

#[test]
fn markdown_mentions_resolve_by_path_then_unique_basename() {
    let files = tree(&[
        "docs/guide.md",
        "docs/notes/deep.md",
        "src/loader.ts",
        "crates/alpha/src/net/client.rs",
        "a/dup.rs",
        "b/dup.rs",
    ]);
    let known = |p: &str| files.contains(p);
    let by_basename = |name: &str| -> Vec<String> {
        files
            .iter()
            .filter(|p| p.rsplit('/').next() == Some(name))
            .cloned()
            .collect()
    };

    // A bare token is a working-tree path first.
    assert_eq!(
        resolve_mention("docs/guide.md", "src/loader.ts", &known, &by_basename),
        Some("src/loader.ts".to_string())
    );
    // A relative token is read against the document's own directory.
    assert_eq!(
        resolve_mention("docs/guide.md", "./notes/deep.md", &known, &by_basename),
        Some("docs/notes/deep.md".to_string())
    );
    assert_eq!(
        resolve_mention("docs/notes/deep.md", "../guide.md", &known, &by_basename),
        Some("docs/guide.md".to_string())
    );
    // Neither shape matches, so a unique basename decides.
    assert_eq!(
        resolve_mention("docs/notes/deep.md", "client.rs", &known, &by_basename),
        Some("crates/alpha/src/net/client.rs".to_string())
    );
    // An ambiguous basename resolves to nothing.
    assert_eq!(
        resolve_mention("docs/guide.md", "dup.rs", &known, &by_basename),
        None
    );
    // So do links that leave the tree.
    assert_eq!(
        resolve_mention(
            "docs/guide.md",
            "https://example.com/a.md",
            &known,
            &by_basename
        ),
        None
    );
}

#[test]
fn markdown_extraction_finds_headings_mentions_and_body() {
    let facts = facts("docs/guide.md", "docs/guide.md");

    assert_eq!(facts.lang, Lang::Markdown);
    assert_eq!(facts.headings, vec!["Guide", "Details", "Wrap-up"]);
    for expected in [
        "src/loader.ts",
        "rust/lib.rs",
        "./notes/deep.md",
        "notes/deep.md",
        // A double-backtick span earlier on the line must not hide this one.
        "notes/after.md",
    ] {
        assert!(
            facts.mentions.iter().any(|m| m == expected),
            "missing mention {expected}: {:?}",
            facts.mentions
        );
    }
    // Fenced code is not prose, and absolute URLs are not mentions.
    assert!(!facts.mentions.iter().any(|m| m == "not/a/mention.rs"));
    assert!(!facts
        .mentions
        .iter()
        .any(|m| m.starts_with("https://") || m == "the site"));
    // Mentions are sorted and deduplicated.
    let mut sorted = facts.mentions.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(facts.mentions, sorted);

    let body = facts.body.expect("markdown keeps its body");
    assert!(body.starts_with("# Guide"));
    assert!(facts.symbols.is_empty());
}

// ── per-language extraction ─────────────────────────────────────────────────

#[test]
fn rust_symbols_are_qualified_with_kinds_docs_and_calls() {
    let facts = facts("rust/lib.rs", "crates/alpha/src/lib.rs");

    assert_eq!(
        names(&facts),
        vec![
            "Record",
            "Record.new",
            "Record.touch",
            "Record.bump",
            "Summary",
            "Summary.summary",
            "Shape",
            "LIMIT",
            "Alias",
            "inner",
            "inner::seed",
            "run",
        ]
    );
    assert_eq!(kind_of(&facts, "Record"), "struct");
    assert_eq!(kind_of(&facts, "Record.new"), "method");
    assert_eq!(kind_of(&facts, "Summary"), "trait");
    assert_eq!(kind_of(&facts, "Summary.summary"), "method");
    assert_eq!(kind_of(&facts, "Shape"), "enum");
    assert_eq!(kind_of(&facts, "LIMIT"), "const");
    assert_eq!(kind_of(&facts, "Alias"), "type");
    assert_eq!(kind_of(&facts, "inner"), "module");
    assert_eq!(kind_of(&facts, "inner::seed"), "function");
    assert_eq!(kind_of(&facts, "run"), "function");

    assert_eq!(doc_of(&facts, "Record"), "A record kept by the crate.");
    assert_eq!(doc_of(&facts, "Record.new"), "Build a record.");
    assert_eq!(doc_of(&facts, "Record.bump"), "");

    assert_eq!(callees(&facts, "Record.new"), vec!["helper"]);
    assert_eq!(callees(&facts, "Record.touch"), vec!["helper", "self.bump"]);
    assert_eq!(callees(&facts, "run"), vec!["Record::new", "r.touch"]);

    assert_eq!(
        raw_imports(&facts),
        vec![
            "mod net",
            "mod util",
            "beta_core::Ledger",
            "crate::util::helper",
            "serde::Serialize",
        ]
    );

    let run = facts.symbols.iter().find(|s| s.name == "run").unwrap();
    assert_eq!(run.signature, "pub fn run(ledger: &Ledger) -> u32");
    assert!(run.line_start < run.line_end);
}

#[test]
fn python_symbols_and_imports_are_extracted() {
    let facts = facts("python/service.py", "app/service.py");

    assert_eq!(
        names(&facts),
        vec!["CAP", "Store", "Store.put", "Store.flush", "main"]
    );
    assert_eq!(kind_of(&facts, "CAP"), "const");
    assert_eq!(kind_of(&facts, "Store"), "class");
    assert_eq!(kind_of(&facts, "Store.put"), "method");
    assert_eq!(kind_of(&facts, "main"), "function");

    assert_eq!(doc_of(&facts, "Store"), "Keeps records in memory.");
    assert_eq!(doc_of(&facts, "Store.put"), "Store one value.");

    assert_eq!(
        callees(&facts, "Store.put"),
        vec!["deep.load", "self.flush"]
    );
    assert!(callees(&facts, "main").contains(&"Store"));

    assert_eq!(
        raw_imports(&facts),
        vec!["os", "pkg.mod_a", "pkg.sub", ".sibling", "..shared"]
    );
}

#[test]
fn typescript_symbols_and_imports_are_extracted() {
    let facts = facts("ts/index.ts", "src/index.ts");

    assert_eq!(
        names(&facts),
        vec![
            "LIMIT",
            "Handle",
            "Sink",
            "Sink.write",
            "Queue",
            "Queue.push",
            "Queue.flush",
            "run",
        ]
    );
    assert_eq!(kind_of(&facts, "LIMIT"), "const");
    assert_eq!(kind_of(&facts, "Handle"), "type");
    assert_eq!(kind_of(&facts, "Sink"), "interface");
    assert_eq!(kind_of(&facts, "Sink.write"), "method");
    assert_eq!(kind_of(&facts, "Queue"), "class");
    assert_eq!(kind_of(&facts, "run"), "function");

    assert_eq!(doc_of(&facts, "Queue"), "A queue of pending jobs.");
    assert_eq!(doc_of(&facts, "Queue.push"), "Push one job.");
    assert_eq!(doc_of(&facts, "LIMIT"), "Largest accepted batch.");

    assert_eq!(callees(&facts, "Queue.push"), vec!["helper", "this.flush"]);
    assert_eq!(callees(&facts, "run"), vec!["path.join", "q.push"]);

    assert_eq!(
        raw_imports(&facts),
        vec![
            "./util",
            "./types",
            "./legacy.js",
            "./widget",
            "node:path",
            "./types",
        ]
    );
}

#[test]
fn tsx_files_parse_with_the_tsx_grammar() {
    let facts = facts("ts/widget/index.tsx", "src/widget/index.tsx");

    assert_eq!(facts.lang, Lang::Tsx);
    assert_eq!(names(&facts), vec!["WidgetProps", "Widget"]);
    assert_eq!(kind_of(&facts, "WidgetProps"), "interface");
    assert_eq!(doc_of(&facts, "Widget"), "Render one widget.");
    assert_eq!(raw_imports(&facts), vec!["../util"]);
}

#[test]
fn javascript_symbols_and_imports_are_extracted() {
    let facts = facts("js/main.js", "src/main.js");

    assert_eq!(names(&facts), vec!["NAME", "Reader", "Reader.read", "boot"]);
    assert_eq!(kind_of(&facts, "Reader"), "class");
    assert_eq!(kind_of(&facts, "Reader.read"), "method");
    assert_eq!(doc_of(&facts, "Reader"), "Reads records by key.");
    assert_eq!(callees(&facts, "Reader.read"), vec!["load", "parse"]);
    // `require` counts as an import alongside the ES module specifier.
    assert_eq!(raw_imports(&facts), vec!["./loader.mjs", "./parser"]);
}

#[test]
fn go_symbols_and_imports_are_extracted() {
    let facts = facts("go/store/store.go", "store/store.go");

    assert_eq!(
        names(&facts),
        vec![
            "Limit",
            "Record",
            "Reader",
            "Store",
            "Store.Put",
            "Store.flush",
            "Open",
        ]
    );
    assert_eq!(kind_of(&facts, "Limit"), "const");
    assert_eq!(kind_of(&facts, "Record"), "struct");
    assert_eq!(kind_of(&facts, "Reader"), "interface");
    assert_eq!(kind_of(&facts, "Store.Put"), "method");
    assert_eq!(kind_of(&facts, "Open"), "function");

    assert_eq!(
        doc_of(&facts, "Limit"),
        "Limit caps the number of stored records."
    );
    assert_eq!(doc_of(&facts, "Store.Put"), "Put writes a record.");
    assert_eq!(doc_of(&facts, "Store.flush"), "");

    assert_eq!(callees(&facts, "Store.Put"), vec!["s.flush", "util.Check"]);
    assert_eq!(raw_imports(&facts), vec!["fmt", "example.com/demo/util"]);
}

// ── invariants ──────────────────────────────────────────────────────────────

#[test]
fn symbols_sorted_by_line_and_qualified() {
    let cases = [
        ("rust/lib.rs", "crates/alpha/src/lib.rs", "Record.new"),
        ("python/service.py", "app/service.py", "Store.put"),
        ("ts/index.ts", "src/index.ts", "Queue.push"),
        ("js/main.js", "src/main.js", "Reader.read"),
        ("go/store/store.go", "store/store.go", "Store.Put"),
    ];
    for (fixture, path, qualified) in cases {
        let facts = facts(fixture, path);
        assert!(
            facts
                .symbols
                .windows(2)
                .all(|pair| pair[0].line_start <= pair[1].line_start
                    && (pair[0].line_start < pair[1].line_start || pair[0].name <= pair[1].name)),
            "{fixture} symbols are not sorted: {:?}",
            names(&facts)
        );
        assert!(
            facts.symbols.iter().any(|s| s.name == qualified),
            "{fixture} is missing the qualified name {qualified}: {:?}",
            names(&facts)
        );
        assert!(
            facts.symbols.iter().all(|s| s.line_start >= 1
                && s.line_end >= s.line_start
                && s.line_end <= facts.lines),
            "{fixture} has a symbol outside the file"
        );
    }
    // A Rust module qualifies with `::`, not `.`.
    let rust = facts("rust/lib.rs", "crates/alpha/src/lib.rs");
    assert!(rust.symbols.iter().any(|s| s.name == "inner::seed"));
}

#[test]
fn calls_attach_to_the_innermost_definition_at_scale() {
    // Five thousand modules, each holding one function that makes one call:
    // ten thousand definitions and five thousand call sites in one file. The
    // shape is what a code generator emits, and it is the input a
    // definition-per-call scan degrades on, so the test is as much about the
    // attachment finishing as about where the calls land.
    const BLOCKS: usize = 5_000;
    let mut source = String::new();
    for i in 0..BLOCKS {
        source.push_str(&format!(
            "pub mod m{i} {{\n    pub fn f{i}() {{\n        g{i}();\n    }}\n}}\n"
        ));
    }
    assert!(
        source.len() < MAX_FILE_BYTES,
        "the generated source must stay under the size cap"
    );

    let facts = extract("src/generated.rs", source.as_bytes());
    assert_eq!(facts.symbols.len(), BLOCKS * 2);

    for i in 0..BLOCKS {
        // The call belongs to the function, which is the innermost
        // definition containing it.
        assert_eq!(
            callees(&facts, &format!("m{i}::f{i}")),
            vec![format!("g{i}").as_str()],
            "call not attached to m{i}::f{i}"
        );
        // Not to the module that merely encloses the function.
        assert!(
            callees(&facts, &format!("m{i}")).is_empty(),
            "call leaked onto the enclosing module m{i}"
        );
    }

    // The same generator at a size the naive quadratic check can afford, so
    // the sweep is pinned against innermost-wins and not just against the
    // expectations above.
    let mut small = String::new();
    for i in 0..200 {
        small.push_str(&format!(
            "pub mod s{i} {{\n    pub fn h{i}() {{\n        k{i}();\n    }}\n}}\n"
        ));
    }
    assert_calls_are_innermost(&extract("src/small.rs", small.as_bytes()), "generated");

    // Nesting a call two definitions deep still picks the innermost, and a
    // call outside every definition is dropped rather than misattached.
    let nested = "\
fn top() {
    outer_call();
}
mod a {
    pub mod b {
        pub fn deep() {
            deep_call();
        }
    }
}
const X: u32 = free_call();
";
    let facts = extract("src/nested.rs", nested.as_bytes());
    assert_eq!(callees(&facts, "top"), vec!["outer_call"]);
    assert_eq!(callees(&facts, "a::b::deep"), vec!["deep_call"]);
    assert!(callees(&facts, "a").is_empty());
    assert!(callees(&facts, "a::b").is_empty());
    // A call in a const initialiser belongs to that const, not to nothing.
    assert_eq!(callees(&facts, "X"), vec!["free_call"]);
}

#[test]
fn hash_is_stable_and_sensitive() {
    let bytes = read_fixture("rust/lib.rs");
    let first = extract("crates/alpha/src/lib.rs", &bytes);
    let second = extract("crates/alpha/src/lib.rs", &bytes);
    assert_eq!(first.hash, second.hash);
    assert_eq!(first.hash.len(), 32);
    assert!(first.hash.chars().all(|c| c.is_ascii_hexdigit()));

    // The path does not feed the hash; the content does.
    assert_eq!(extract("elsewhere/other.rs", &bytes).hash, first.hash);

    let mut changed = bytes.clone();
    changed.push(b'\n');
    assert_ne!(
        extract("crates/alpha/src/lib.rs", &changed).hash,
        first.hash
    );

    // One flipped byte is enough.
    let mut flipped = bytes;
    flipped[0] ^= 0x01;
    assert_ne!(
        extract("crates/alpha/src/lib.rs", &flipped).hash,
        first.hash
    );

    assert_eq!(extract("empty.rs", b"").lines, 0);
}

#[test]
fn body_cut_is_utf8_safe() {
    // A three-byte character makes the byte limit fall mid-character, so the
    // cut has to back up to a boundary.
    let prefix = "# T\n\n";
    let mut doc = String::from(prefix);
    while doc.len() < MAX_BODY_BYTES + 3_000 {
        doc.push('€');
    }
    let facts = extract("docs/big.md", doc.as_bytes());
    let body = facts.body.expect("markdown keeps its body");

    assert!(body.len() <= MAX_BODY_BYTES);
    assert!(body.len() > MAX_BODY_BYTES - 4, "cut backed up too far");
    assert_ne!(
        body.len(),
        MAX_BODY_BYTES,
        "the cut should land off the limit"
    );
    assert!(doc.starts_with(&body));
    assert!(std::str::from_utf8(body.as_bytes()).is_ok());
}

#[test]
fn never_panics_on_binary_and_huge_input() {
    // A PNG header: binary from its first bytes.
    let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00];
    let facts = extract("assets/logo.png", &png);
    assert_eq!(facts.lang, Lang::Other);
    assert!(facts.symbols.is_empty() && facts.body.is_none());
    assert_eq!(facts.hash.len(), 32);

    // Source-shaped path, binary content.
    let mut sneaky = b"fn main() {}\n".to_vec();
    sneaky.push(0);
    sneaky.extend_from_slice(b"more");
    let facts = extract("src/main.rs", &sneaky);
    assert_eq!(facts.lang, Lang::Rust);
    assert!(facts.symbols.is_empty(), "binary content yields no symbols");
    assert_eq!(facts.hash.len(), 32);

    // Invalid UTF-8 that has no NUL byte.
    let facts = extract("src/main.rs", &[b'f', b'n', 0xff, 0xfe, b'\n']);
    assert!(facts.symbols.is_empty());
    assert_eq!(facts.lines, 1);

    // Over the size limit: hash, language and lines only.
    let mut huge = Vec::with_capacity(2 * MAX_FILE_BYTES);
    while huge.len() < 2 * MAX_FILE_BYTES {
        huge.extend_from_slice(b"pub fn generated_symbol_name() -> u32 { 1 }\n");
    }
    let facts = extract("src/huge.rs", &huge);
    assert_eq!(facts.lang, Lang::Rust);
    assert!(facts.symbols.is_empty(), "an oversized file is hash-only");
    assert!(facts.imports.is_empty());
    assert_eq!(facts.hash.len(), 32);
    assert!(facts.lines > 0);

    // A pathologically nested `use` tree must not recurse into the stack.
    let deep = format!("use {}a{};\n", "a::{".repeat(20_000), "}".repeat(20_000));
    let facts = extract("src/deep.rs", deep.as_bytes());
    assert_eq!(facts.hash.len(), 32);

    // Degenerate inputs.
    for (path, bytes) in [
        ("", b"".as_slice()),
        ("no-extension", b"anything"),
        ("src/empty.rs", b""),
        ("docs/empty.md", b""),
        ("src/broken.rs", b"fn ( { { { unterminated"),
        ("app/broken.py", b"def (:\n  ???"),
        ("src/broken.ts", b"class { function ("),
        ("store/broken.go", b"func ("),
    ] {
        let facts = extract(path, bytes);
        assert_eq!(facts.hash.len(), 32, "{path}");
    }
}

#[test]
fn extract_of_this_repo_is_deterministic() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .join("cli/src");
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    assert!(
        files.len() > 2,
        "expected real sources under crates/cli/src"
    );

    let run = |files: &[PathBuf]| -> Vec<FileFacts> {
        files
            .iter()
            .map(|path| {
                let rel = path.to_string_lossy().replace('\\', "/");
                let bytes = std::fs::read(path).expect("read source");
                extract(&rel, &bytes)
            })
            .collect()
    };
    assert_eq!(run(&files), run(&files));

    // And the walk found real structure, not just hashes.
    let total: usize = run(&files).iter().map(|f| f.symbols.len()).sum();
    assert!(total > 0, "expected symbols in crates/cli/src");
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect(&path, out);
        } else if lang_of(&path.to_string_lossy()) != Lang::Other {
            out.push(path);
        }
    }
}

#[test]
fn resolve_call_prefers_same_file_then_directory_then_unique() {
    let mut index = SymbolIndex::new();
    index.insert("flush", "src/a.rs#Store.flush");
    index.insert("flush", "src/b.rs#Cache.flush");
    index.insert("flush", "other/c.rs#Sink.flush");
    index.insert("only", "far/away.rs#only");
    index.insert("Store.flush", "src/a.rs#Store.flush");

    // Same file wins, even reached through a receiver.
    assert_eq!(
        resolve_call("src/a.rs", "self.flush", &index),
        Some("src/a.rs#Store.flush".to_string())
    );
    // The fully written name is tried before its last segment.
    assert_eq!(
        resolve_call("other/c.rs", "Store.flush", &index),
        Some("src/a.rs#Store.flush".to_string())
    );
    // No definition in this file, two in this directory: ambiguous.
    assert_eq!(resolve_call("src/c.rs", "flush", &index), None);
    // Unique anywhere in the tree.
    assert_eq!(
        resolve_call("src/a.rs", "only", &index),
        Some("far/away.rs#only".to_string())
    );
    assert_eq!(resolve_call("src/a.rs", "missing", &index), None);
    assert_eq!(index.len(), 3);
    assert!(!index.is_empty());
}

#[test]
fn extraction_is_capped_and_repeatable_for_every_fixture() {
    let cases = [
        ("rust/lib.rs", "crates/alpha/src/lib.rs"),
        ("rust/client.rs", "crates/alpha/src/net/client.rs"),
        ("python/service.py", "app/service.py"),
        ("python/sibling.py", "app/sibling.py"),
        ("ts/index.ts", "src/index.ts"),
        ("ts/widget/index.tsx", "src/widget/index.tsx"),
        ("js/main.js", "src/main.js"),
        ("go/store/store.go", "store/store.go"),
        ("docs/guide.md", "docs/guide.md"),
        ("docs/notes/deep.md", "docs/notes/deep.md"),
    ];
    for (fixture, path) in cases {
        let first = facts(fixture, path);
        assert_eq!(first, facts(fixture, path), "{fixture} is not repeatable");
        assert_calls_are_innermost(&first, fixture);
        for symbol in &first.symbols {
            assert!(symbol.signature.chars().count() <= 200, "{fixture}");
            assert!(symbol.doc.chars().count() <= 200, "{fixture}");
            assert!(symbol.calls.len() <= 256, "{fixture}");
            let mut sorted = symbol.calls.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(symbol.calls, sorted, "{fixture} calls are not canonical");
        }
        let mut imports = first.imports.clone();
        imports.sort_by(|a, b| (a.line, &a.raw).cmp(&(b.line, &b.raw)));
        assert_eq!(first.imports, imports, "{fixture} imports are not sorted");
    }
}

/// A module's own description is not a description of whatever happens to sit
/// below it. Rust writes one as `//!` and Python as a bare string statement at
/// the top of the file; in both cases the first definition below is
/// undocumented until it carries a doc of its own.
#[test]
fn module_doc_is_not_attributed_to_first_item() {
    // Rust, blank line or not: an inner doc comment never documents an item.
    let spaced = extract(
        "src/util.rs",
        b"//! Shared helpers.\n\npub fn helper() -> u32 {\n    2\n}\n",
    );
    assert_eq!(doc_of(&spaced, "helper"), "");
    let touching = extract(
        "src/util.rs",
        b"//! Shared helpers.\npub fn helper() -> u32 {\n    2\n}\n",
    );
    assert_eq!(doc_of(&touching, "helper"), "");
    let block = extract(
        "src/util.rs",
        b"/*! Shared helpers. */\npub fn helper() -> u32 {\n    2\n}\n",
    );
    assert_eq!(doc_of(&block, "helper"), "");

    // And the item's own `///` still wins, even one blank line under a `//!`.
    let owned = extract(
        "src/util.rs",
        b"//! Shared helpers.\n\n/// Double a value.\npub fn helper(n: u32) -> u32 {\n    n * 2\n}\n",
    );
    assert_eq!(doc_of(&owned, "helper"), "Double a value.");

    // Python: the module docstring belongs to the module.
    let module = extract(
        "app/tick.py",
        b"\"\"\"Timing helpers.\"\"\"\n\n\ndef tick():\n    return 1\n",
    );
    assert_eq!(doc_of(&module, "tick"), "");
    let documented = extract(
        "app/tick.py",
        b"\"\"\"Timing helpers.\"\"\"\n\n\ndef tick():\n    \"\"\"Return a tick.\"\"\"\n    return 1\n",
    );
    assert_eq!(doc_of(&documented, "tick"), "Return a tick.");

    // The same holds where a file header is an ordinary comment or a block
    // comment separated from the first definition.
    let go = extract(
        "store/store.go",
        b"// Package store keeps records.\npackage store\n\nfunc Open() {}\n",
    );
    assert_eq!(doc_of(&go, "Open"), "");
    let ts = extract(
        "src/index.ts",
        b"/** Module header. */\n\nexport function run(): number {\n    return 1;\n}\n",
    );
    assert_eq!(doc_of(&ts, "run"), "");
}

/// A blank line between an outer doc comment and its item breaks the
/// association — the fix that makes the `//!` case above reachable at all.
#[test]
fn a_blank_line_detaches_an_outer_doc_comment() {
    let detached = extract(
        "src/util.rs",
        b"/// Double a value.\n\npub fn helper() -> u32 {\n    2\n}\n",
    );
    assert_eq!(doc_of(&detached, "helper"), "");
    let attached = extract(
        "src/util.rs",
        b"/// Double a value.\npub fn helper() -> u32 {\n    2\n}\n",
    );
    assert_eq!(doc_of(&attached, "helper"), "Double a value.");
    // A run of `///` lines still reads from the top of the run.
    let run = extract(
        "src/util.rs",
        b"/// First line.\n/// Second line.\npub fn helper() -> u32 {\n    2\n}\n",
    );
    assert_eq!(doc_of(&run, "helper"), "First line.");
}
