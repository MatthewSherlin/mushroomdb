//! A synthetic code graph, shaped exactly like what `ingest-git` writes.
//!
//! Shared by every `repograph` suite. Nothing here reads a real repository:
//! the paths, authors, shas and hashes are invented, and every number is a
//! function of the file's index, so two builds of this store are identical.
//!
//! # Shape
//!
//! | Label | n | Notes |
//! |---|---|---|
//! | `File` | 30 | 12 in `src/core`, 10 in `src/web`, 8 in `tests` |
//! | `Symbol` | 12 | `calls_to` crosses directories, so `CALLS` does too |
//! | `Author` | 4 | named; `TOP_AUTHOR` in-degree 12 / 10 / 7 / 1 |
//! | `Commit` | 40 | `ts` spans 5 quarters, 8 commits each |
//! | `Concept` | 2 | one whose `source_hashes` no longer match its file |
//! | `Note` | 1 | `about` the busiest file |
//! | `GitSync` | 1 | `sha` of the newest commit |
//!
//! Each directory's files import that directory's first file, so `IMPORTS`
//! alone partitions the graph into three components. Every commit touches the
//! first three files of one directory plus one rotating other, so `CO_CHANGED`
//! reinforces the same three groups.

// Every integration test is its own crate and includes this file as a private
// module, so whatever one suite does not call is dead code in that suite's
// build. The fixture is shared, so that is expected rather than a warning.
#![allow(dead_code)]

use core_api::{default_max_edges, Direction, GraphDb, Predicate, RuleDef, Value};
use std::path::{Path, PathBuf};

/// Seconds in a day, and in the quarter the commit clock advances by.
pub const DAY_SECS: i64 = 86_400;
const QUARTER: i64 = 91 * DAY_SECS;
/// `ts` of the oldest commit. An arbitrary fixed epoch — nothing reads a clock.
pub const T0: i64 = 1_600_000_000;
/// The `GitSync` marker's `synced_at`: twelve minutes after the newest commit,
/// so a test that passes `now_ts: Some(SYNCED_AT + 720)` reads `12m ago`.
pub const SYNCED_AT: i64 = T0 + 4 * QUARTER + 7 * DAY_SECS + 60;
/// Commits per quarter, over 5 quarters.
const PER_QUARTER: usize = 8;
/// Total commits.
pub const COMMITS: usize = 40;

/// The three directories and how many files each holds.
pub const DIRS: [(&str, usize, &str); 3] = [
    ("src/core", 12, "c"),
    ("src/web", 10, "w"),
    ("tests", 8, "t"),
];

/// A scratch directory under the system temp dir, unique per process and call.
#[must_use]
pub fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-repograph-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Open an empty store at `dir`.
#[must_use]
pub fn open(dir: &Path) -> GraphDb<core_storage::fs::RealFs> {
    GraphDb::open(dir).expect("open")
}

/// The key of file `i` in directory `d`.
#[must_use]
pub fn file_key(d: usize, i: usize) -> String {
    let (dir, _, stem) = DIRS[d];
    format!("{dir}/{stem}{i:02}.rs")
}

/// Every file key, in the order the store is built.
#[must_use]
pub fn all_files() -> Vec<String> {
    let mut out = Vec::new();
    for (d, &(_, n, _)) in DIRS.iter().enumerate() {
        for i in 0..n {
            out.push(file_key(d, i));
        }
    }
    out
}

/// The sha of commit `i`. Forty hex characters, distinct in the first seven so
/// an abbreviated sha still identifies it.
#[must_use]
pub fn sha(i: usize) -> String {
    format!("{:07x}{:033x}", 0x00a1_b2c3usize + i * 7919, i)
}

/// The `ts` of commit `i`: eight commits per quarter, one day apart.
#[must_use]
pub fn commit_ts(i: usize) -> i64 {
    T0 + (i / PER_QUARTER) as i64 * QUARTER + (i % PER_QUARTER) as i64 * DAY_SECS
}

/// The newest commit's `ts` — what [`repo_map`](core_api::repograph::repo_map)
/// takes as "now" when no `now_ts` is given.
#[must_use]
pub fn newest_ts() -> i64 {
    commit_ts(COMMITS - 1)
}

/// The files commit `i` touches: the first three of its directory, plus one
/// that rotates through the rest.
#[must_use]
pub fn touched(i: usize) -> Vec<String> {
    let d = i % DIRS.len();
    let n = DIRS[d].1;
    let mut files: Vec<String> = (0..3).map(|k| file_key(d, k)).collect();
    files.push(file_key(d, 3 + (i / DIRS.len()) % (n - 3)));
    files
}

/// The invented content hash of a file.
#[must_use]
pub fn hash_of(key: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The author who owns file `i` of directory `d`. The last file of `tests`
/// belongs to the fourth author, so every author owns at least one file.
#[must_use]
pub fn owner_of(d: usize, i: usize) -> &'static str {
    match (d, i) {
        (2, 7) => "d@example.test",
        (0, _) => "a@example.test",
        (1, _) => "b@example.test",
        _ => "c@example.test",
    }
}

/// The four authors, `(key, name)`.
pub const AUTHORS: [(&str, &str); 4] = [
    ("a@example.test", "Ada Example"),
    ("b@example.test", "Bea Example"),
    ("c@example.test", "Cy Example"),
    ("d@example.test", "Dee Example"),
];

/// The twelve symbols: `(file dir, file index, name, callee index)`. A callee
/// index points into this same table; `None` means the symbol calls nothing.
const SYMBOLS: [(usize, usize, &str, Option<usize>); 12] = [
    (0, 0, "core::init", None),
    (0, 1, "core::run", Some(0)),
    (0, 2, "core::stop", Some(0)),
    (0, 3, "core::load", Some(1)),
    (0, 4, "core::save", Some(1)),
    (0, 5, "core::flush", Some(4)),
    (1, 0, "web::serve", Some(0)),
    (1, 1, "web::route", Some(6)),
    (1, 2, "web::render", Some(1)),
    (1, 3, "web::auth", Some(2)),
    (2, 0, "tests::smoke", Some(6)),
    (2, 1, "tests::api", Some(7)),
];

/// The key of symbol `n` of [`SYMBOLS`].
fn symbol_key(n: usize) -> String {
    let (d, i, name, _) = SYMBOLS[n];
    format!("{}#{name}", file_key(d, i))
}

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

fn list(items: &[String]) -> Value {
    Value::List(items.iter().map(|i| s(i)).collect())
}

/// Build the store described in the module docs at `dir`.
///
/// Nodes go in first and rules last, mirroring `ingest-git`: a rule backfills
/// once, on creation, so every derived edge exists by the time this returns.
#[must_use]
pub fn synthetic_repo_store(dir: &Path) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = open(dir);

    for (key, name) in AUTHORS {
        db.insert_node("Author", key, vec![("name".into(), s(name))])
            .expect("author");
    }

    // Which commits touched each file, and which author wrote most of them.
    let mut commits_of: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for i in 0..COMMITS {
        for f in touched(i) {
            commits_of.entry(f).or_default().push(sha(i));
        }
    }

    for (d, &(dir_name, n, _)) in DIRS.iter().enumerate() {
        for i in 0..n {
            let key = file_key(d, i);
            let mut props = vec![
                ("id".into(), s(&key)),
                ("path".into(), s(&key)),
                ("dir".into(), s(dir_name)),
                ("ext".into(), s("rs")),
                ("lang".into(), s("rust")),
                ("hash".into(), s(&hash_of(&key))),
                ("lines".into(), Value::Int(40 + i as i64)),
                ("top_author_id".into(), s(owner_of(d, i))),
            ];
            // Every file but the first of its directory imports that first
            // file, and quotes the line it did so on.
            if i > 0 {
                let target = file_key(d, 0);
                props.push(("imports".into(), list(std::slice::from_ref(&target))));
                props.push((
                    "import_lines".into(),
                    list(&[format!("{target}\t{}", 3 + i)]),
                ));
            }
            let cs = commits_of.get(&key).cloned().unwrap_or_default();
            if !cs.is_empty() {
                props.push(("n_commits".into(), Value::Int(cs.len() as i64)));
                props.push(("commits".into(), list(&cs)));
            }
            db.insert_node("File", &key, props).expect("file");
        }
    }

    for (n, &(d, i, name, callee)) in SYMBOLS.iter().enumerate() {
        let key = symbol_key(n);
        let file = file_key(d, i);
        let mut props = vec![
            ("id".into(), s(&key)),
            ("name".into(), s(name)),
            ("kind".into(), s("function")),
            ("path".into(), s(&file)),
            ("file_id".into(), s(&file)),
            ("line_start".into(), Value::Int(10 + n as i64)),
            ("line_end".into(), Value::Int(20 + n as i64)),
            ("signature".into(), s(&format!("fn {name}()"))),
            ("doc".into(), s(&format!("what {name} does"))),
        ];
        if let Some(c) = callee {
            let target = symbol_key(c);
            props.push(("calls_to".into(), list(std::slice::from_ref(&target))));
            props.push((
                "call_lines".into(),
                list(&[format!("{target}\t{}", 12 + n)]),
            ));
        }
        db.insert_node("Symbol", &key, props).expect("symbol");
    }

    for i in 0..COMMITS {
        let key = sha(i);
        db.insert_node(
            "Commit",
            &key,
            vec![
                ("id".into(), s(&key)),
                ("message".into(), s(&format!("change {i:02}"))),
                ("ts".into(), Value::Int(commit_ts(i))),
                ("author_id".into(), s(AUTHORS[i % AUTHORS.len()].0)),
            ],
        )
        .expect("commit");
    }
    for i in 0..COMMITS {
        for f in touched(i) {
            db.insert_edge("TOUCHED", &sha(i), &f).expect("touched");
        }
    }

    // One concept still agrees with the file it was learned from; the other
    // records a hash that file no longer has.
    let fresh = file_key(0, 0);
    db.insert_node(
        "Concept",
        "concept:startup",
        vec![
            ("id".into(), s("concept:startup")),
            ("name".into(), s("startup path")),
            ("summary".into(), s("how the core boots")),
            ("source_files".into(), list(std::slice::from_ref(&fresh))),
            ("source_hashes".into(), list(&[hash_of(&fresh)])),
            ("extracted_by".into(), s("agent")),
            ("extracted_at".into(), Value::Int(newest_ts())),
        ],
    )
    .expect("concept");
    let stale = file_key(1, 0);
    db.insert_node(
        "Concept",
        "concept:routing",
        vec![
            ("id".into(), s("concept:routing")),
            ("name".into(), s("routing")),
            ("summary".into(), s("how requests are routed")),
            ("source_files".into(), list(&[stale])),
            (
                "source_hashes".into(),
                list(&["0000000000000000".to_string()]),
            ),
            ("extracted_by".into(), s("agent")),
            ("extracted_at".into(), Value::Int(T0)),
        ],
    )
    .expect("concept");

    db.insert_node(
        "Note",
        "note:0001",
        vec![
            ("id".into(), s("note:0001")),
            (
                "text".into(),
                s("the core entry point is worth reading first"),
            ),
            ("kind".into(), s("note")),
            ("ts".into(), Value::Int(newest_ts())),
            ("source".into(), s("agent")),
            ("about".into(), list(&[file_key(0, 0)])),
        ],
    )
    .expect("note");

    db.insert_node(
        "GitSync",
        "__mushroomdb_git_sync__",
        vec![
            ("id".into(), s("__mushroomdb_git_sync__")),
            ("sha".into(), s(&sha(COMMITS - 1))),
            ("synced_at".into(), Value::Int(SYNCED_AT)),
            ("repo".into(), s("/synthetic/repo")),
            ("recurse".into(), Value::Bool(false)),
            ("prs".into(), Value::Bool(false)),
            ("structure".into(), Value::Bool(true)),
            ("docs".into(), Value::Bool(true)),
        ],
    )
    .expect("gitsync");

    for def in rules() {
        db.create_rule(def).expect("rule");
    }
    db
}

/// The rules `ingest-git` and `structure` declare, recreated here because
/// `core-api` cannot depend on the CLI crate that owns them. Kept in the same
/// creation order, with the same predicates, edge types and fan-outs.
#[must_use]
pub fn rules() -> Vec<RuleDef> {
    let mut out = vec![
        key_rule(
            "auto_fk_symbol_file_id",
            "Symbol",
            "File",
            "file_id",
            "DEFINES",
        ),
        key_rule("imports", "File", "File", "imports", "IMPORTS"),
        key_rule("calls", "Symbol", "Symbol", "calls_to", "CALLS"),
        key_rule("mentions", "File", "File", "mentions", "MENTIONS"),
        key_rule(
            "concept_sources",
            "Concept",
            "File",
            "source_files",
            "DESCRIBED_IN",
        ),
        key_rule(
            "auto_fk_commit_author_id",
            "Commit",
            "Author",
            "author_id",
            "AUTHOR",
        ),
        key_rule(
            "auto_fk_file_top_author_id",
            "File",
            "Author",
            "top_author_id",
            "TOP_AUTHOR",
        ),
    ];
    for label in ["Author", "Concept", "File", "Note", "Symbol"] {
        out.push(key_rule(
            &format!("about_{}", label.to_lowercase()),
            "Note",
            label,
            "about",
            "ABOUT",
        ));
    }
    let co = Predicate::Overlap {
        field: "commits".into(),
        min: 0.25,
    };
    out.push(RuleDef {
        name: "co_changed".into(),
        src_label: "File".into(),
        dst_label: "File".into(),
        predicate: co.clone(),
        edge_type: "CO_CHANGED".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(10),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    });
    out.push(RuleDef {
        name: "knows".into(),
        src_label: "Author".into(),
        dst_label: "File".into(),
        predicate: co,
        edge_type: "KNOWS".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(20),
        approximate: false,
        via_label: Some("File".into()),
        via_edge: Some("TOP_AUTHOR".into()),
        via_dir: Some(Direction::In),
    });
    out
}

fn key_rule(name: &str, src: &str, dst: &str, field: &str, edge: &str) -> RuleDef {
    let predicate = Predicate::KeyMatch {
        field: field.into(),
    };
    let max_edges = Some(default_max_edges(&predicate));
    RuleDef {
        name: name.into(),
        src_label: src.into(),
        dst_label: dst.into(),
        predicate,
        edge_type: edge.into(),
        weight_prop: None,
        max_edges,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}
