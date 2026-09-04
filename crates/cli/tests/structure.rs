//! Structure ingest: the working tree read as symbols, imports, calls and
//! documentation mentions, on top of the history graph `ingest-git` builds.
//!
//! Every fixture here is a synthetic tree written into a throwaway git
//! repository by the test itself, so the assertions do not depend on the
//! contents of any real project.
use cli::ingest_git::{run_ingest_git, IngestGitOpts};
use core_api::{Direction, GraphDb, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Unique per call: tests run concurrently and two of them can read the same
/// nanosecond, which would otherwise hand both the same repo.
static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!(
        "mushroomdb-structure-{name}-{}-{nanos}-{seq}",
        std::process::id()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn git(repo: &Path, args: &[&str]) {
    let st = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?} failed");
}

fn write_files(repo: &Path, files: &[(&str, &str)]) {
    for (p, body) in files {
        let full = repo.join(p);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }
}

fn commit_all(repo: &Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "-c",
            "user.name=alice",
            "-c",
            "user.email=alice@x.test",
            "commit",
            "-q",
            "-m",
            msg,
        ],
    );
}

fn commit(repo: &Path, msg: &str, files: &[(&str, &str)]) {
    write_files(repo, files);
    commit_all(repo, msg);
}

const LIB_RS: &str = "//! Demo crate root.

mod net;
mod util;

use crate::util::helper;

/// A record kept by the crate.
pub struct Record {
    pub id: u32,
}

/// Run the demo.
pub fn run() -> u32 {
    helper(1)
}
";

const UTIL_RS: &str = "//! Shared helpers.

/// Double a value.
pub fn helper(n: u32) -> u32 {
    n * 2
}
";

const NET_RS: &str = "//! Networking.

use crate::util::helper;

/// Open a connection.
pub fn connect(port: u32) -> u32 {
    helper(port)
}
";

/// `use crate::util::helper;` removed; `connect` still calls `helper`, so the
/// `IMPORTS` edge retracts while the `CALLS` edge stays.
const NET_RS_NO_IMPORT: &str = "//! Networking.

/// Open a connection.
pub fn connect(port: u32) -> u32 {
    helper(port)
}
";

const GUIDE_MD: &str = "# Demo guide

The networking layer lives in `src/net.rs` and is quicksilver by design.

## Wiring

See [the helpers](../src/util.rs) for the doubling routine.
";

/// A three-module Rust crate plus one Markdown guide, committed once.
fn seed_repo() -> PathBuf {
    let repo = tmp("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(
        &repo,
        "demo crate",
        &[
            ("Cargo.toml", "[package]\nname = \"demo\"\n"),
            ("src/lib.rs", LIB_RS),
            ("src/util.rs", UTIL_RS),
            ("src/net.rs", NET_RS),
            ("docs/guide.md", GUIDE_MD),
        ],
    );
    repo
}

fn opts(repo: &Path) -> IngestGitOpts {
    IngestGitOpts {
        repo: repo.to_path_buf(),
        exclude: vec![],
        max_commits_per_file: 200,
        recurse_submodules: false,
        prs: false,
        structure: true,
        docs: true,
        ensure_gitignore: false,
    }
}

fn prop(db: &cli::structure::Db, key: &str, field: &str) -> Option<Value> {
    db.node_ref(key).and_then(|n| n.prop(field))
}

fn strings(v: Option<Value>) -> Vec<String> {
    match v {
        Some(Value::List(l)) => l
            .into_iter()
            .filter_map(|x| match x {
                Value::Str(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn out(db: &cli::structure::Db, key: &str, edge: &str) -> Vec<String> {
    let mut v = db.neighbors(key, edge, Direction::Out).unwrap_or_default();
    v.sort();
    v
}

fn all_keys(db: &cli::structure::Db, label: &str) -> Vec<String> {
    let rs = db
        .query(
            &format!("MATCH (n:{label}) RETURN n.id AS id"),
            &BTreeMap::new(),
        )
        .unwrap();
    let mut v: Vec<String> = (0..rs.len())
        .filter_map(|i| match rs.get(i, "id") {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    v.sort();
    v
}

#[test]
fn first_run_creates_symbols_imports_calls_and_mentions() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    let report = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert!(report.structure.files_scanned >= 5, "{report:?}");

    let db = GraphDb::open(&db_dir).unwrap();

    // IMPORTS: `mod` declarations from the crate root, `use crate::…` from a
    // module. Both resolve to working-tree files, so both become edges.
    assert_eq!(
        out(&db, "src/lib.rs", "IMPORTS"),
        vec!["src/net.rs".to_string(), "src/util.rs".to_string()]
    );
    assert_eq!(
        out(&db, "src/net.rs", "IMPORTS"),
        vec!["src/util.rs".to_string()]
    );

    // Symbols, keyed `<path>#<qualified name>`, with DEFINES back to the file.
    assert!(db.has_node("src/util.rs#helper"), "helper symbol missing");
    assert_eq!(
        prop(&db, "src/util.rs#helper", "kind"),
        Some(Value::Str("function".into()))
    );
    assert_eq!(
        prop(&db, "src/util.rs#helper", "file_id"),
        Some(Value::Str("src/util.rs".into()))
    );
    assert_eq!(
        prop(&db, "src/util.rs#helper", "doc"),
        Some(Value::Str("Double a value.".into()))
    );
    assert_eq!(
        out(&db, "src/util.rs#helper", "DEFINES"),
        vec!["src/util.rs".to_string()]
    );

    // CALLS: `connect` calls `helper`, which is defined in a sibling file.
    assert_eq!(
        out(&db, "src/net.rs#connect", "CALLS"),
        vec!["src/util.rs#helper".to_string()]
    );

    // MENTIONS: a backticked path and a relative link in the guide.
    assert_eq!(
        out(&db, "docs/guide.md", "MENTIONS"),
        vec!["src/net.rs".to_string(), "src/util.rs".to_string()]
    );

    // `symbols_n` and the content hash ride along on the file.
    assert_eq!(prop(&db, "src/util.rs", "symbols_n"), Some(Value::Int(1)));
    assert_eq!(
        prop(&db, "src/util.rs", "lang"),
        Some(Value::Str("rust".into()))
    );
    let Some(Value::Str(hash)) = prop(&db, "src/util.rs", "hash") else {
        panic!("no hash on src/util.rs");
    };
    assert_eq!(hash.len(), 32, "hash is 32 hex characters: {hash}");

    // `explain` names the rule behind the edge.
    let ex = db.explain("src/net.rs", "src/util.rs").unwrap();
    assert!(
        ex.iter()
            .any(|e| e.rule == "imports" && e.edge_type == "IMPORTS"),
        "explain must name the imports rule: {ex:?}"
    );
}

#[test]
fn editing_an_import_retracts_the_edge_on_refresh() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    {
        let db = GraphDb::open(&db_dir).unwrap();
        assert_eq!(
            out(&db, "src/net.rs", "IMPORTS"),
            vec!["src/util.rs".to_string()]
        );
    }

    commit(&repo, "drop the use", &[("src/net.rs", NET_RS_NO_IMPORT)]);
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(
        out(&db, "src/net.rs", "IMPORTS").is_empty(),
        "the import was deleted, so the edge must retract"
    );
    assert!(
        !strings(prop(&db, "src/net.rs", "imports")).contains(&"src/util.rs".to_string()),
        "the imports list must be rewritten, not just the edge"
    );

    // The call survives: `resolve_call` works off the symbol index, not the
    // import list, so dropping the `use` does not unlink the callee.
    assert_eq!(
        out(&db, "src/net.rs#connect", "CALLS"),
        vec!["src/util.rs#helper".to_string()]
    );

    let hist = db.edge_history("src/net.rs", "src/util.rs").unwrap();
    let imports: Vec<&core_api::EdgeHistoryEvent> = hist
        .items
        .iter()
        .filter(|e| e.edge_type == "IMPORTS")
        .collect();
    assert!(
        imports
            .iter()
            .any(|e| e.event == core_api::EdgeEvent::Added && e.rule.as_deref() == Some("imports")),
        "history must show the edge being added: {imports:?}"
    );
    assert!(
        imports
            .iter()
            .any(|e| e.event == core_api::EdgeEvent::Retracted),
        "history must show the edge being retracted: {imports:?}"
    );
}

#[test]
fn renamed_file_moves_its_symbols() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    git(&repo, &["mv", "src/util.rs", "src/helpers.rs"]);
    commit_all(&repo, "move util to helpers");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(!db.has_node("src/util.rs"), "the old path is gone");
    assert!(db.has_node("src/helpers.rs"), "the new path exists");

    // Symbols are keyed by path, so they are deleted and re-created.
    assert!(
        !db.has_node("src/util.rs#helper"),
        "the old symbol key must not survive the rename"
    );
    assert!(db.has_node("src/helpers.rs#helper"), "symbol must move");
    assert_eq!(
        prop(&db, "src/helpers.rs#helper", "file_id"),
        Some(Value::Str("src/helpers.rs".into()))
    );
    assert_eq!(
        out(&db, "src/helpers.rs#helper", "DEFINES"),
        vec!["src/helpers.rs".to_string()]
    );
    assert_eq!(
        all_keys(&db, "Symbol")
            .iter()
            .filter(|k| k.starts_with("src/util.rs#"))
            .count(),
        0,
        "no symbol may be left under the old path"
    );

    // The importer of the old path is re-extracted, so nothing dangles: the
    // `use crate::util::helper` in net.rs no longer names a file.
    assert!(
        out(&db, "src/net.rs", "IMPORTS").is_empty(),
        "an unresolvable import must leave no edge, stale or retargeted"
    );
    assert!(
        !strings(prop(&db, "src/net.rs", "imports")).contains(&"src/util.rs".to_string()),
        "the importer's list must no longer name the old key"
    );

    // The call follows the symbol to its new key.
    assert_eq!(
        out(&db, "src/net.rs#connect", "CALLS"),
        vec!["src/helpers.rs#helper".to_string()]
    );
}

#[test]
fn no_structure_skips_symbols_and_hashes() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    let mut o = opts(&repo);
    o.structure = false;
    o.docs = false;
    let report = run_ingest_git(&db_dir, &o).unwrap();
    assert_eq!(report.structure.files_scanned, 0);

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("src/util.rs"), "history is still ingested");
    assert_eq!(prop(&db, "src/util.rs", "hash"), None, "no content hash");
    assert_eq!(prop(&db, "src/util.rs", "symbols_n"), None);
    assert!(all_keys(&db, "Symbol").is_empty(), "no symbols");
    assert!(
        out(&db, "src/net.rs", "IMPORTS").is_empty(),
        "no derived structure edges"
    );
}

#[test]
fn markdown_body_is_fulltext_searchable() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    let db = GraphDb::open(&db_dir).unwrap();
    assert_eq!(
        strings(prop(&db, "docs/guide.md", "headings")),
        vec!["Demo guide".to_string(), "Wiring".to_string()]
    );
    let hits = db.search("body", "quicksilver");
    assert_eq!(
        hits.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        vec!["docs/guide.md".to_string()],
        "the guide body must be searchable"
    );
    let by_heading = db.search("headings", "wiring");
    assert!(
        by_heading.iter().any(|(k, _)| k == "docs/guide.md"),
        "headings are indexed too: {by_heading:?}"
    );

    // Without --docs the prose is not stored at all.
    let db_dir2 = tmp("db-nodocs");
    let mut o = opts(&repo);
    o.docs = false;
    run_ingest_git(&db_dir2, &o).unwrap();
    let db2 = GraphDb::open(&db_dir2).unwrap();
    assert_eq!(prop(&db2, "docs/guide.md", "body"), None);
    assert_eq!(prop(&db2, "docs/guide.md", "headings"), None);
    assert!(
        out(&db2, "docs/guide.md", "MENTIONS").is_empty(),
        "no mentions without --docs"
    );
    assert!(
        prop(&db2, "docs/guide.md", "hash").is_some(),
        "the hash is structure, not docs"
    );
}

#[test]
fn large_and_binary_files_are_hash_only() {
    let repo = seed_repo();
    let big = format!("// {}\npub fn huge() {{}}\n", "x".repeat(1024 * 1024));
    write_files(&repo, &[("src/big.rs", &big)]);
    let mut binary = b"pub fn nope() {}\n".to_vec();
    binary.extend_from_slice(&[0u8, 1, 2, 3, 0]);
    std::fs::write(repo.join("src/blob.rs"), &binary).unwrap();
    commit_all(&repo, "add a huge file and a binary one");

    let db_dir = tmp("db");
    let report = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!(
        report.structure.skipped_large, 2,
        "both files are hash-only: {report:?}"
    );

    let db = GraphDb::open(&db_dir).unwrap();
    for path in ["src/big.rs", "src/blob.rs"] {
        assert!(
            prop(&db, path, "hash").is_some(),
            "{path} must still be hashed"
        );
        assert_eq!(
            prop(&db, path, "symbols_n"),
            Some(Value::Int(0)),
            "{path} must contribute no symbols"
        );
        assert_eq!(prop(&db, path, "imports"), None, "{path} has no imports");
    }
    assert!(
        !db.has_node("src/big.rs#huge"),
        "no symbol from a huge file"
    );
    assert!(
        !db.has_node("src/blob.rs#nope"),
        "no symbol from binary bytes"
    );
}

/// The hash is what makes a sync cheap: a file whose bytes did not change is
/// scanned, found identical, and left entirely alone.
#[test]
fn unchanged_files_are_not_rewritten() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    let before_file = {
        let db = GraphDb::open(&db_dir).unwrap();
        db.node_history("src/util.rs").unwrap().len()
    };
    let before_commits = core_api::wal_commit_count_at(&db_dir).unwrap();

    // A second full pass over the same working tree scans every file and
    // writes none of them.
    let report = {
        let shared = core_api::SharedDb::open(&db_dir).unwrap();
        let mut w = shared.write();
        cli::structure::refresh_all(&mut w, &repo, "", true).unwrap()
    };
    assert_eq!(report.files_scanned, 5, "every file is read again");

    assert_eq!(
        core_api::wal_commit_count_at(&db_dir).unwrap(),
        before_commits,
        "a re-scan of an unchanged tree must not add a single WAL commit"
    );
    let db = GraphDb::open(&db_dir).unwrap();
    assert_eq!(
        db.node_history("src/util.rs").unwrap().len(),
        before_file,
        "an unchanged file must produce no write on a re-scan"
    );
}

/// The last commit of a run is the sync marker. Anything written after it
/// would be work the marker already claims to cover, so a failure in that work
/// would be skipped forever: the next run would resume from a sha past it.
#[test]
fn the_sync_marker_is_written_after_the_structure_pass() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    let db = GraphDb::open(&db_dir).unwrap();
    let newest = |key: &str| {
        db.node_history(key)
            .unwrap()
            .iter()
            .map(|e| e.commit)
            .max()
            .unwrap_or_else(|| panic!("no history for {key}"))
    };
    let marker = newest("__mushroomdb_git_sync__");
    for key in [
        "src/util.rs",
        "src/net.rs",
        "docs/guide.md",
        "src/util.rs#helper",
    ] {
        assert!(
            newest(key) < marker,
            "{key} was written at commit {} but the marker landed at {marker}",
            newest(key)
        );
    }
}

/// The failure the marker ordering protects against is a structure batch that
/// will not commit. An unreadable file is *not* one of those: the pass skips
/// what it cannot read and the run still succeeds, so the marker advances and
/// the next run does not retry. This pins that deliberate degradation.
#[cfg(unix)]
#[test]
fn an_unreadable_file_is_skipped_rather_than_failing_the_run() {
    use std::os::unix::fs::PermissionsExt;

    let repo = seed_repo();
    let db_dir = tmp("db");
    let locked = repo.join("src/net.rs");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read(&locked).is_err();
    // Running as root, or on a filesystem that ignores the mode bits, makes
    // the file readable anyway; there is nothing to assert then.
    if !readable {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        return;
    }

    let report = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        report.structure.files_scanned, 4,
        "the unreadable file is skipped, the other four are read: {report:?}"
    );

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("src/net.rs"), "its history is still ingested");
    assert_eq!(
        prop(&db, "src/net.rs", "hash"),
        None,
        "but nothing was read"
    );
    assert!(!db.has_node("src/net.rs#connect"));
    assert!(
        db.has_node("__mushroomdb_git_sync__"),
        "the run succeeded, so the marker advanced"
    );
}

/// `#` is legal in a path, so a file can be named exactly what another file's
/// symbol key would be. Whichever node holds the key keeps it: overwriting a
/// `File` node with symbol props would corrupt it.
#[test]
fn a_symbol_key_that_collides_with_a_file_leaves_the_file_alone() {
    let repo = seed_repo();
    write_files(&repo, &[("src/util.rs#helper", "not a symbol\n")]);
    commit_all(&repo, "a file named like a symbol key");

    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    let db = GraphDb::open(&db_dir).unwrap();
    assert_eq!(
        prop(&db, "src/util.rs#helper", "path"),
        Some(Value::Str("src/util.rs#helper".into())),
        "the File node keeps its own path"
    );
    assert_eq!(
        prop(&db, "src/util.rs#helper", "file_id"),
        None,
        "no symbol props were written onto it"
    );
    assert_eq!(prop(&db, "src/util.rs#helper", "kind"), None);
    // The rest of the file is unaffected: `helper` simply has no Symbol node.
    assert_eq!(prop(&db, "src/util.rs", "symbols_n"), Some(Value::Int(1)));
    assert_eq!(
        out(&db, "src/net.rs", "IMPORTS"),
        vec!["src/util.rs".to_string()]
    );
}

/// Turning a flag back on has to revisit files the previous run deliberately
/// skipped, even though not one byte of them changed since.
#[test]
fn turning_docs_back_on_fills_in_the_prose() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    let mut o = opts(&repo);
    o.docs = false;
    run_ingest_git(&db_dir, &o).unwrap();
    {
        let db = GraphDb::open(&db_dir).unwrap();
        assert_eq!(prop(&db, "docs/guide.md", "body"), None);
    }

    // Same commits, same bytes, different flag.
    let report = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!(
        report.structure.files_scanned, 5,
        "a full re-scan: {report:?}"
    );

    {
        // Scoped: an open handle holds the store's write lock, so the next
        // ingest would find the database busy.
        let db = GraphDb::open(&db_dir).unwrap();
        assert!(prop(&db, "docs/guide.md", "body").is_some(), "body arrives");
        assert_eq!(
            out(&db, "docs/guide.md", "MENTIONS"),
            vec!["src/net.rs".to_string(), "src/util.rs".to_string()]
        );
    }

    // And back off again retracts it, rather than leaving prose behind.
    run_ingest_git(&db_dir, &o).unwrap();
    let db = GraphDb::open(&db_dir).unwrap();
    assert_eq!(prop(&db, "docs/guide.md", "body"), None);
    assert!(out(&db, "docs/guide.md", "MENTIONS").is_empty());
}

#[test]
fn import_lines_are_recorded() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    let db = GraphDb::open(&db_dir).unwrap();
    assert_eq!(
        strings(prop(&db, "src/net.rs", "import_lines")),
        vec!["src/util.rs\t3".to_string()],
        "the evidence line is the line the `use` is written on"
    );
    assert_eq!(
        strings(prop(&db, "src/lib.rs", "import_lines")),
        vec![
            "src/net.rs\t3".to_string(),
            "src/util.rs\t4".to_string(),
            "src/util.rs\t6".to_string(),
        ],
        "one entry per import site, sorted"
    );
    assert_eq!(
        strings(prop(&db, "src/net.rs#connect", "call_lines")),
        vec!["src/util.rs#helper\t7".to_string()],
        "call evidence carries the call site line"
    );
}
