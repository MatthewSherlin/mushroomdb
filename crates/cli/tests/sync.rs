//! `sync` and `touch`: keeping a codebase graph current between full ingests.
//!
//! `sync` is what a git hook runs after a commit — an incremental `ingest-git`
//! plus a working-tree pass over whatever is still dirty. `touch` is what an
//! editor hook runs after one file changes, and re-extracts only that file.
//!
//! Also covers the two small resolvers the same surface needs: `--auto`
//! database discovery and the version string.
use cli::ingest_git::{run_ingest_git, run_sync, run_touch, IngestGitOpts};
use cli::{resolve_auto_db, version_string};
use core_api::{Direction, GraphDb};
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
        "mushroomdb-sync-{name}-{}-{nanos}-{seq}",
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

fn commit(repo: &Path, msg: &str, files: &[(&str, &str)]) {
    write_files(repo, files);
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

const LIB_RS: &str = "//! Demo crate root.

mod net;
mod util;

/// Run the demo.
pub fn run() -> u32 {
    3
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

/// `use crate::util::helper;` removed, so the `IMPORTS` edge must retract.
const NET_RS_NO_IMPORT: &str = "//! Networking.

/// Open a connection.
pub fn connect(port: u32) -> u32 {
    2
}
";

/// A three-module Rust crate, committed once.
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
        ],
    );
    repo
}

fn opts(repo: &Path) -> IngestGitOpts {
    IngestGitOpts {
        repo: repo.to_path_buf(),
        exclude: cli::ingest_git::DEFAULT_EXCLUDES
            .iter()
            .map(|p| (*p).to_string())
            .collect(),
        max_commits_per_file: cli::ingest_git::DEFAULT_MAX_COMMITS_PER_FILE,
        recurse_submodules: false,
        prs: false,
        structure: true,
        docs: true,
        ensure_gitignore: false,
    }
}

fn out(db: &cli::structure::Db, key: &str, edge: &str) -> Vec<String> {
    let mut v = db.neighbors(key, edge, Direction::Out).unwrap_or_default();
    v.sort();
    v
}

// ── sync ────────────────────────────────────────────────────────────────────

/// `sync` re-stamps the marker when it takes new commits, so `map` can say how
/// stale the graph is rather than how old its newest commit is.
#[test]
fn sync_restamps_the_marker_when_it_takes_new_commits() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    let stamp = |dir: &Path| match GraphDb::open(dir)
        .unwrap()
        .node_ref("__mushroomdb_git_sync__")
        .unwrap()
        .prop("synced_at")
    {
        Some(core_api::Value::Int(at)) => at,
        other => panic!("synced_at must be an integer, got {other:?}"),
    };
    let first = stamp(&db_dir);

    // Nothing new: the whole run writes nothing, the stamp included.
    let seq = GraphDb::open(&db_dir).unwrap().commit_seq();
    run_sync(&db_dir).unwrap();
    assert_eq!(GraphDb::open(&db_dir).unwrap().commit_seq(), seq);
    assert_eq!(stamp(&db_dir), first);

    // One new commit, and the sync dates itself.
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    commit(&repo, "add extra", &[("src/extra.rs", "//! Extra.\n")]);
    let r = run_sync(&db_dir).unwrap();
    assert_eq!(r.git.commits, 1, "{r:?}");
    assert!(stamp(&db_dir) > first, "the sync re-stamped the marker");
}

/// A hook-driven `sync` has to do both halves of the job: walk the commits
/// that landed since the marker, and re-extract the files the working tree has
/// changed but not committed. Neither half alone keeps the graph honest.
#[test]
fn sync_after_new_commit_is_incremental_and_refreshes_dirty_working_tree() {
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

    // One new commit, plus one file dirtied in the working tree afterwards.
    commit(
        &repo,
        "add extra",
        &[(
            "src/extra.rs",
            "//! Extra.\n\npub fn extra() -> u32 {\n    1\n}\n",
        )],
    );
    write_files(&repo, &[("src/net.rs", NET_RS_NO_IMPORT)]);

    let r = run_sync(&db_dir).unwrap();
    assert!(r.git.incremental, "the marker was already there: {r:?}");
    assert_eq!(r.git.commits, 1, "exactly the one new commit: {r:?}");
    assert_eq!(
        r.dirty_refreshed, 1,
        "src/net.rs is the only dirty path: {r:?}"
    );
    assert_eq!(r.structure.files_scanned, 1, "{r:?}");

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("src/extra.rs"), "the new commit was walked");
    assert!(
        out(&db, "src/net.rs", "IMPORTS").is_empty(),
        "the uncommitted edit must retract the import edge"
    );
    drop(db);

    // Nothing new to do: no commits, and the working tree is no longer dirty
    // relative to the last refresh — but net.rs is still uncommitted, so it is
    // scanned again and found unchanged, which writes nothing.
    let again = run_sync(&db_dir).unwrap();
    assert_eq!(again.git.commits, 0, "no new commits: {again:?}");
    assert_eq!(again.git.files, 0, "{again:?}");
}

/// A database that was never pointed at a repository cannot guess one.
#[test]
fn sync_without_repo_prop_errors_clearly() {
    let db_dir = tmp("db");
    // A real store, just one that has never seen `ingest-git`.
    drop(GraphDb::open(&db_dir).unwrap());

    let err = run_sync(&db_dir).expect_err("no marker, so no repository");
    assert!(
        err.0.contains("no git sync marker") && err.0.contains("ingest-git"),
        "the message must name the fix: {}",
        err.0
    );
}

/// `sync` writes, so it serialises on the store's cross-process lock like every
/// other writer: exit 3 and a retry message, with nothing written.
#[test]
fn sync_reports_busy_when_lock_held() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    commit(
        &repo,
        "another",
        &[("src/util.rs", "//! Shared helpers v2.\n")],
    );

    // A plain read-write handle holds the lock for as long as it lives, so the
    // child process cannot get it.
    let holder = GraphDb::open(&db_dir).unwrap();
    let seq_before = holder.commit_seq();

    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("sync")
        .arg(&db_dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "Busy is exit code 3");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another mushroomdb process is writing; retry"),
        "got: {stderr}"
    );
    assert_eq!(
        holder.commit_seq(),
        seq_before,
        "the busy run wrote nothing"
    );
    drop(holder);

    // Once the lock is free the same sync succeeds.
    let r = run_sync(&db_dir).unwrap();
    assert_eq!(r.git.commits, 1);
}

// ── snapshots ───────────────────────────────────────────────────────────────

fn wal_len(db_dir: &Path) -> u64 {
    std::fs::metadata(db_dir.join("wal.bin")).map_or(0, |m| m.len())
}

/// Enough Markdown to push one incremental run's WAL tail past
/// [`cli::ingest_git::SNAPSHOT_WAL_BYTES`]. Each file stores a body prop, so
/// the WAL grows roughly with the bytes committed.
fn bulky_docs() -> Vec<(String, String)> {
    let para = "Nodes and edges and rules and files and symbols and commits. ".repeat(1_000);
    (0..96)
        .map(|n| {
            (
                format!("docs/page{n}.md"),
                format!("# Page {n}\n\n{para}\n"),
            )
        })
        .collect()
}

/// A first ingest is the run that writes the whole history into an empty WAL.
/// Leaving that WAL to be replayed makes every later open — every hook, every
/// MCP start — pay for it, so the run that wrote it snapshots it away.
#[test]
fn full_ingest_writes_a_snapshot() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    assert!(
        db_dir.join("snapshot.bin").is_file(),
        "a full ingest leaves a snapshot behind"
    );
    // The default snapshot replaces the WAL with a minimal baseline, so a
    // reopen replays almost nothing.
    assert!(
        wal_len(&db_dir) < cli::ingest_git::SNAPSHOT_WAL_BYTES,
        "the WAL was replaced by a baseline, not left whole: {} bytes",
        wal_len(&db_dir)
    );
    // And the store still reads correctly through that snapshot — including
    // the past. The WAL frames were archived, not dropped, so a store does not
    // forget how its nodes came to be in exchange for opening faster.
    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("src/net.rs"), "the graph survived the snapshot");
    assert_eq!(out(&db, "src/net.rs", "IMPORTS"), vec!["src/util.rs"]);
    assert!(
        !db.node_history("src/net.rs").unwrap().is_empty(),
        "the snapshot kept the history readable"
    );
    assert!(
        db.edge_history("src/net.rs", "src/util.rs")
            .unwrap()
            .items
            .iter()
            .any(|e| e.edge_type == "IMPORTS"),
        "the IMPORTS edge is still explainable after the snapshot"
    );
}

/// An incremental run snapshots only when the tail it appended is long enough
/// to be worth the write. A small sync leaves the snapshot alone; a large one
/// folds itself in.
#[test]
fn incremental_sync_snapshots_past_the_threshold() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    let first = std::fs::read(db_dir.join("snapshot.bin")).unwrap();

    // A small commit: under the threshold, so the snapshot is not rewritten.
    commit(&repo, "one more", &[("src/extra.rs", "//! Extra.\n")]);
    run_sync(&db_dir).unwrap();
    assert_eq!(
        std::fs::read(db_dir.join("snapshot.bin")).unwrap(),
        first,
        "a small incremental run is not worth a snapshot"
    );
    let small_tail = wal_len(&db_dir);
    assert!(small_tail > 0, "the increment is in the WAL");

    // A bulky one: the tail clears the threshold and is folded in.
    let docs = bulky_docs();
    let files: Vec<(&str, &str)> = docs.iter().map(|(p, b)| (p.as_str(), b.as_str())).collect();
    commit(&repo, "a pile of docs", &files);
    run_sync(&db_dir).unwrap();

    let after = wal_len(&db_dir);
    assert!(
        after < small_tail.max(cli::ingest_git::SNAPSHOT_WAL_BYTES),
        "the long tail was folded into the snapshot, leaving {after} bytes"
    );
    assert!(
        std::fs::metadata(db_dir.join("snapshot.bin"))
            .unwrap()
            .len()
            > first.len() as u64,
        "the snapshot grew to hold what the WAL no longer carries"
    );
    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("docs/page0.md"), "nothing was lost");
}

/// `touch` runs on every edit the assistant makes, inside the hook's budget.
/// A snapshot costs hundreds of milliseconds, so it never takes one — even
/// when the store has none at all and one would otherwise be due.
#[test]
fn touch_never_snapshots() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    // Get the files into the graph, then rewrite every one of them through
    // `touch` alone. That pushes the WAL tail past the threshold, so a
    // snapshot is unambiguously due by the time the last edit lands — and only
    // the rule that `touch` never takes one can keep it away.
    let docs = bulky_docs();
    let files: Vec<(&str, &str)> = docs.iter().map(|(p, b)| (p.as_str(), b.as_str())).collect();
    commit(&repo, "a pile of docs", &files);
    run_sync(&db_dir).unwrap();
    let before = std::fs::read(db_dir.join("snapshot.bin")).unwrap();

    let edited: Vec<(String, String)> = docs
        .iter()
        .map(|(p, b)| (p.clone(), format!("{b}\nEdited by the assistant.\n")))
        .collect();
    write_files(
        &repo,
        &edited
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_str()))
            .collect::<Vec<_>>(),
    );
    let named: Vec<PathBuf> = edited.iter().map(|(p, _)| repo.join(p)).collect();
    let r = run_touch(&db_dir, &named, None).unwrap();
    assert_eq!(r.files_scanned, docs.len(), "every edit was taken: {r:?}");

    assert!(
        wal_len(&db_dir) > cli::ingest_git::SNAPSHOT_WAL_BYTES,
        "a snapshot is genuinely due: {} bytes of tail",
        wal_len(&db_dir)
    );
    assert_eq!(
        std::fs::read(db_dir.join("snapshot.bin")).unwrap(),
        before,
        "touch stays inside the hook budget and writes no snapshot"
    );
}

// ── touch ───────────────────────────────────────────────────────────────────

/// Run the real binary with `stdin` piped in, and return
/// `(exit code, stdout, stderr)`.
fn run_bin(args: &[&str], stdin: &str) -> (Option<i32>, String, String) {
    use std::io::Write as _;
    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A `PostToolUse` hook fires on every edit the assistant makes, and whatever
/// it writes lands in the user's session. Pointed at a database that was never
/// built from a repository — the common case for a hook installed globally —
/// it must say nothing at all and exit 0.
#[test]
fn touch_hook_mode_is_silent_on_missing_marker() {
    let db_dir = tmp("db");
    drop(GraphDb::open(&db_dir).unwrap()); // a real store, but no ingest-git
    let payload = format!(
        r#"{{"tool_name":"Edit","tool_input":{{"file_path":{}}}}}"#,
        serde_json::to_string(&db_dir.join("x.rs").to_string_lossy().into_owned()).unwrap()
    );

    // Hook mode is "no files on the command line", with or without --auto.
    let (code, stdout, stderr) = run_bin(&["touch", &db_dir.to_string_lossy()], &payload);
    assert_eq!(code, Some(0), "a hook must never fail the tool call");
    assert_eq!(stdout, "", "hook mode prints nothing on stdout");
    assert_eq!(stderr, "", "hook mode prints nothing on stderr");

    // A database directory that does not exist at all is just as silent, and
    // must not be created on the way past.
    let missing = db_dir.join("nope").join("deeper");
    let (code, stdout, stderr) = run_bin(&["touch", &missing.to_string_lossy()], &payload);
    assert_eq!(code, Some(0));
    assert_eq!(stdout, "");
    assert_eq!(stderr, "");
    assert!(!missing.exists(), "a hook must not seed a database");

    // A person naming files on the command line still gets the error, because
    // they are owed an answer and nothing is reading their stdout.
    let (code, _stdout, stderr) = run_bin(
        &[
            "touch",
            &db_dir.to_string_lossy(),
            &db_dir.join("x.rs").to_string_lossy(),
        ],
        "",
    );
    assert_eq!(code, Some(1), "explicit files: a real exit code");
    assert!(
        stderr.contains("no git sync marker"),
        "explicit files: a real message, got {stderr:?}"
    );
}

/// Anything at all can arrive on a hook's stdin, including nothing. None of it
/// is worth a word of output.
#[test]
fn touch_hook_mode_is_silent_on_garbage_payload() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    let db = db_dir.to_string_lossy().into_owned();

    for payload in [
        "",
        "not json at all",
        "{",
        r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#,
        r#"{"tool_input":{"file_path":""}}"#,
        r#"{"tool_input":{"file_path":"/etc/hosts"}}"#,
        r#"{"tool_input":{"file_path":12345}}"#,
        r#"{"tool_input":{"edits":"not an array"}}"#,
        r#"[1,2,3]"#,
    ] {
        let (code, stdout, stderr) = run_bin(&["touch", &db], payload);
        assert_eq!(code, Some(0), "payload {payload:?}");
        assert_eq!(stdout, "", "payload {payload:?}");
        assert_eq!(stderr, "", "payload {payload:?}");
    }

    // A payload that *does* name a known file is equally silent, and still did
    // the work: the import it dropped is gone from the graph.
    write_files(&repo, &[("src/net.rs", NET_RS_NO_IMPORT)]);
    let payload = format!(
        r#"{{"tool_input":{{"file_path":{}}}}}"#,
        serde_json::to_string(&repo.join("src/net.rs").to_string_lossy().into_owned()).unwrap()
    );
    let (code, stdout, stderr) = run_bin(&["touch", &db], &payload);
    assert_eq!(code, Some(0));
    assert_eq!(stdout, "", "a successful hook fire is noise too");
    assert_eq!(stderr, "");
    let graph = GraphDb::open(&db_dir).unwrap();
    assert!(
        out(&graph, "src/net.rs", "IMPORTS").is_empty(),
        "silent does not mean idle"
    );
}

/// The other hook body, held to the same standard at the process boundary.
/// `run_recall` is silent on every error by contract, but nothing checked that
/// the binary around it is.
#[test]
fn recall_hook_is_silent_and_exits_zero() {
    let db_dir = tmp("db");
    let missing = db_dir.join("never-created");
    for (dir, payload) in [
        (&missing, r#"{"prompt":"anything"}"#),
        (&db_dir, "not json"),
        (&db_dir, ""),
        (&db_dir, r#"{"prompt":"nothing here matches"}"#),
    ] {
        let (code, stdout, stderr) = run_bin(&["recall", &dir.to_string_lossy()], payload);
        assert_eq!(code, Some(0), "recall must never block a prompt");
        assert_eq!(stdout, "", "payload {payload:?}");
        assert_eq!(stderr, "", "payload {payload:?}");
    }
    assert!(!missing.exists(), "recall must not seed a database");
}

/// The single-file path: one edit, one re-extraction, and the edges the old
/// content derived are gone.
#[test]
fn touch_reextracts_one_file_and_retracts_removed_import() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    write_files(&repo, &[("src/net.rs", NET_RS_NO_IMPORT)]);
    let report = run_touch(&db_dir, &[repo.join("src/net.rs")], None).unwrap();
    assert_eq!(
        report.files_scanned, 1,
        "exactly the touched file: {report:?}"
    );

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(
        out(&db, "src/net.rs", "IMPORTS").is_empty(),
        "the removed `use` must retract the edge"
    );
    // Nothing else was re-extracted: lib.rs still imports both modules.
    assert_eq!(
        out(&db, "src/lib.rs", "IMPORTS"),
        vec!["src/net.rs".to_string(), "src/util.rs".to_string()]
    );
}

/// Driven from a `PostToolUse` payload the same way `recall` is driven from a
/// `UserPromptSubmit` one: the path comes off stdin, not argv.
#[test]
fn touch_reads_file_path_from_hook_payload() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    write_files(&repo, &[("src/net.rs", NET_RS_NO_IMPORT)]);

    let path = repo.join("src/net.rs");
    let payload = format!(
        r#"{{"tool_name":"Edit","tool_input":{{"file_path":{}}}}}"#,
        serde_json::to_string(&path.to_string_lossy().into_owned()).unwrap()
    );
    let report = run_touch(&db_dir, &[], Some(&payload)).unwrap();
    assert_eq!(report.files_scanned, 1, "{report:?}");

    {
        let db = GraphDb::open(&db_dir).unwrap();
        assert!(out(&db, "src/net.rs", "IMPORTS").is_empty());
    }

    // The MultiEdit shape carries the paths one level deeper.
    write_files(&repo, &[("src/util.rs", "//! Shared helpers.\n")]);
    let payload = format!(
        r#"{{"tool_name":"MultiEdit","tool_input":{{"edits":[{{"file_path":{}}}]}}}}"#,
        serde_json::to_string(&repo.join("src/util.rs").to_string_lossy().into_owned()).unwrap()
    );
    let report = run_touch(&db_dir, &[], Some(&payload)).unwrap();
    assert_eq!(report.files_scanned, 1, "{report:?}");
    let db = GraphDb::open(&db_dir).unwrap();
    assert!(
        !db.has_node("src/util.rs#helper"),
        "the deleted function's symbol must be swept"
    );

    // Payloads with nothing usable in them are a silent no-op, never an error.
    assert_eq!(
        run_touch(&db_dir, &[], Some("not json"))
            .unwrap()
            .files_scanned,
        0
    );
    assert_eq!(
        run_touch(&db_dir, &[], Some(r#"{"tool_name":"Bash"}"#))
            .unwrap()
            .files_scanned,
        0
    );
}

/// A hook fires on every edit the assistant makes, most of which are nowhere
/// near this repository. Those must cost nothing and write nothing.
#[test]
fn touch_ignores_paths_outside_repo() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    let seq_before = GraphDb::open(&db_dir).unwrap().commit_seq();

    let elsewhere = tmp("elsewhere");
    std::fs::write(elsewhere.join("stray.rs"), "pub fn stray() {}\n").unwrap();
    let report = run_touch(
        &db_dir,
        &[
            elsewhere.join("stray.rs"),
            PathBuf::from("/etc/hosts"),
            // Inside the repo but excluded by the default patterns.
            repo.join("target/debug/build.rs"),
            // Inside the repo but not a file the graph knows.
            repo.join("src/never-existed.rs"),
        ],
        None,
    )
    .unwrap();
    assert_eq!(report.files_scanned, 0, "{report:?}");
    assert_eq!(
        GraphDb::open(&db_dir).unwrap().commit_seq(),
        seq_before,
        "an out-of-repo touch must not write"
    );
}

// ── --auto and --version ────────────────────────────────────────────────────

/// The resolution order a hook relies on when it is handed no path at all.
#[test]
fn auto_db_prefers_project_dir_then_git_cwd_then_home() {
    let project = tmp("project");
    let cwd = tmp("cwd");
    let home = tmp("home");

    // 1. `$CLAUDE_PROJECT_DIR` wins outright, git repository or not.
    assert_eq!(
        resolve_auto_db(Some(project.as_os_str()), &cwd, &home),
        project.join("mushroom-memory")
    );

    // 2. No env var, but the working directory is a git checkout.
    assert_eq!(
        resolve_auto_db(None, &cwd, &home),
        home.join(".mushroomdb").join("memory"),
        "a cwd without .git falls through to home"
    );
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    assert_eq!(
        resolve_auto_db(None, &cwd, &home),
        cwd.join("mushroom-memory")
    );

    // 3. An empty env var is not a value.
    assert_eq!(
        resolve_auto_db(Some(std::ffi::OsStr::new("")), &cwd, &home),
        cwd.join("mushroom-memory")
    );
}

/// Every checkout of a repository resolves to its own store, and every
/// subdirectory of a checkout resolves to that checkout's.
///
/// This is what makes committed config safe. `install --project` writes
/// `--auto` into `.mcp.json`, the settings hooks and the git hooks; those
/// files travel to a `git worktree`, and a store path baked into them would
/// point every hook in the new worktree at the old checkout's graph.
#[test]
fn auto_db_resolves_to_the_worktree_root() {
    let repo = tmp("wt-main");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, "first", &[("src/lib.rs", LIB_RS)]);
    let home = tmp("wt-home");

    // The root of the main checkout, and any subdirectory of it.
    assert_eq!(
        resolve_auto_db(None, &repo, &home),
        repo.join("mushroom-memory")
    );
    assert_eq!(
        resolve_auto_db(None, &repo.join("src"), &home),
        repo.join("mushroom-memory"),
        "a hook fires wherever the tool call was, which is often a subdirectory"
    );

    // A linked worktree: its own root, never the checkout it was added from.
    let wt = tmp("wt-linked").join("feature");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
        ],
    );
    assert!(
        wt.join(".git").is_file(),
        "a linked worktree marks its root with a .git file, not a directory"
    );
    assert_eq!(
        resolve_auto_db(None, &wt, &home),
        wt.join("mushroom-memory")
    );
    assert_eq!(
        resolve_auto_db(None, &wt.join("src"), &home),
        wt.join("mushroom-memory")
    );
    assert_ne!(
        resolve_auto_db(None, &wt, &home),
        resolve_auto_db(None, &repo, &home),
        "two working trees are two stores"
    );
}

/// The `post-commit` block `install` writes, run for real from inside a
/// linked worktree, updates that worktree's store and leaves the main
/// checkout's alone.
///
/// Worktrees share one hooks directory, so this is literally the same file
/// running in both places — the only thing that can tell them apart is that
/// `sync --auto` resolves the store when it runs.
#[test]
fn git_hook_sync_auto_uses_the_worktree_store() {
    use cli::install::{merge_git_hook, StoreRef};

    let repo = tmp("hook-main");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, "first", &[("src/lib.rs", LIB_RS)]);

    let wt = tmp("hook-wt").join("feature");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
        ],
    );

    // Each checkout starts with its own store, built from its own files.
    let main_db = repo.join("mushroom-memory");
    let wt_db = wt.join("mushroom-memory");
    run_ingest_git(&main_db, &opts(&repo)).unwrap();
    run_ingest_git(&wt_db, &opts(&wt)).unwrap();
    let main_seq_before = GraphDb::open(&main_db).unwrap().commit_seq();
    let wt_seq_before = GraphDb::open(&wt_db).unwrap().commit_seq();

    // Exactly the block install writes, with the test binary as the command.
    let bin = env!("CARGO_BIN_EXE_mushroomdb");
    let hook = repo.join(".git").join("hooks").join("post-commit");
    let block_written =
        merge_git_hook(&hook, &format!("'{bin}'"), &StoreRef::auto(main_db.clone())).unwrap();
    assert!(block_written);
    assert!(
        std::fs::read_to_string(&hook)
            .unwrap()
            .contains("sync --auto"),
        "the block resolves the store at run time"
    );

    // Commit in the worktree. The hook is backgrounded, so wait for it.
    commit(&wt, "only in the worktree", &[("src/feature.rs", NET_RS)]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if GraphDb::open(&wt_db).unwrap().commit_seq() > wt_seq_before {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the post-commit sync never reached {}",
            wt_db.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert_eq!(
        GraphDb::open(&main_db).unwrap().commit_seq(),
        main_seq_before,
        "the other checkout's store must not be touched"
    );
}

/// `install` run from inside a linked worktree puts its git hooks where git
/// will actually run them: the repository's **common** hooks directory.
///
/// A linked worktree's gitdir is `<main>/.git/worktrees/<name>`, and that is
/// what the `gitdir:` link in its `.git` file points at — but git resolves
/// hooks through the common dir, so a hook written into the worktree's own
/// gitdir is a file nothing ever executes. This installs from the worktree,
/// makes a real commit there, and requires the hook to have fired.
#[test]
fn install_from_a_worktree_writes_hooks_to_the_common_dir() {
    use cli::doctor::{run_doctor_with, DoctorOpts};
    use cli::install::{
        run_install_with, Externals, InstallOpts, McpCommand, Platform, Scope, HOOK_BEGIN,
    };

    let repo = tmp("wt-hooks-main");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, "first", &[("src/lib.rs", LIB_RS)]);

    let wt = tmp("wt-hooks-linked").join("feature");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
        ],
    );
    assert!(
        wt.join(".git").is_file(),
        "a linked worktree marks its root with a .git file, not a directory"
    );

    // Each checkout has its own store, so a hook that fires is visible as a
    // commit on that checkout's graph and nowhere else.
    let main_db = repo.join("mushroom-memory");
    let wt_db = wt.join("mushroom-memory");
    run_ingest_git(&main_db, &opts(&repo)).unwrap();
    run_ingest_git(&wt_db, &opts(&wt)).unwrap();
    let main_seq_before = GraphDb::open(&main_db).unwrap().commit_seq();
    let wt_seq_before = GraphDb::open(&wt_db).unwrap().commit_seq();

    let home = tmp("wt-hooks-home");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));
    let install_opts = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::Project),
        db: None, // `--auto`: each checkout resolves its own store
        command: Some(bin.clone()),
        git_hooks: true,
        prewarm: false,
    };
    let summary = run_install_with(
        &wt,
        &home,
        &install_opts,
        &McpCommand::Explicit(bin.clone()),
        &Externals::with_path(None),
    )
    .expect("install from the worktree");

    // The block is in the common dir, and the worktree's own gitdir holds no
    // hooks at all — writing one there is the whole bug.
    let common_hook = repo.join(".git").join("hooks").join("post-commit");
    let block = std::fs::read_to_string(&common_hook).expect("post-commit in the common hooks dir");
    assert!(
        block.contains(HOOK_BEGIN),
        "the block is in {common_hook:?}"
    );
    assert!(
        block.contains("sync --auto"),
        "the store resolves at run time, per checkout"
    );
    let wt_gitdir = repo.join(".git").join("worktrees").join("feature");
    assert!(wt_gitdir.is_dir(), "the worktree's gitdir exists");
    assert!(
        !wt_gitdir.join("hooks").exists(),
        "nothing may be written to the worktree's own gitdir: git never reads it"
    );
    assert!(
        summary.contains(&common_hook.display().to_string()),
        "the summary names the file git will run:\n{summary}"
    );

    // `doctor` must read the same directory, or the failure this fixes stays
    // invisible from the tool that exists to catch it.
    let report = run_doctor_with(
        &wt,
        &home,
        &DoctorOpts {
            platform: Some(Platform::ClaudeCode),
            scope: Some(Scope::Project),
        },
        &Externals::with_path(None),
    )
    .expect("doctor");
    let git_line = report
        .output
        .lines()
        .find(|l| l.contains("git-hooks"))
        .unwrap_or_else(|| panic!("no git-hooks line in:\n{}", report.output));
    assert!(
        git_line.starts_with("ok"),
        "git-hooks should be ok: {git_line}"
    );
    assert!(
        git_line.contains(&repo.join(".git").join("hooks").display().to_string()),
        "doctor reports the common hooks dir: {git_line}"
    );

    // The real thing: commit in the worktree and require the hook to run.
    commit(&wt, "only in the worktree", &[("src/feature.rs", NET_RS)]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if GraphDb::open(&wt_db).unwrap().commit_seq() > wt_seq_before {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the post-commit hook never reached {}",
            wt_db.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        GraphDb::open(&main_db).unwrap().commit_seq(),
        main_seq_before,
        "the other checkout's store must not be touched"
    );

    // Installing again from the main checkout targets the same file, so the
    // block must be merged in place rather than appended a second time.
    let main_opts = InstallOpts {
        db: None,
        ..install_opts
    };
    run_install_with(
        &repo,
        &home,
        &main_opts,
        &McpCommand::Explicit(bin),
        &Externals::with_path(None),
    )
    .expect("install from the main checkout");
    let after = std::fs::read_to_string(&common_hook).unwrap();
    assert_eq!(
        after.matches(HOOK_BEGIN).count(),
        1,
        "one block per hook, however many checkouts installed:\n{after}"
    );
}

/// A submodule keeps its own hooks: its gitdir has no `commondir`, and git
/// runs `.git/modules/<path>/hooks` for it.
///
/// The two shapes are told apart by that one file, so this is the other half
/// of [`install_from_a_worktree_writes_hooks_to_the_common_dir`]. The gitdir
/// link is written by hand rather than by `git submodule add`, which needs a
/// clonable origin and is blocked over local paths by default on current git.
#[test]
fn install_in_a_submodule_keeps_the_submodule_hooks() {
    use cli::install::{
        run_install_with, Externals, InstallOpts, McpCommand, Platform, Scope, HOOK_BEGIN,
    };

    let repo = tmp("sub-hooks-main");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, "first", &[("src/lib.rs", LIB_RS)]);

    // Exactly the shape git leaves for a submodule: a gitdir link with no
    // `commondir` beside it.
    let module = repo.join(".git").join("modules").join("vendor").join("sub");
    std::fs::create_dir_all(&module).unwrap();
    let sub = repo.join("vendor").join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(".git"), format!("gitdir: {}\n", module.display())).unwrap();
    assert!(
        !module.join("commondir").exists(),
        "a submodule's gitdir has no commondir — that is the discriminator"
    );

    let home = tmp("sub-hooks-home");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));
    run_install_with(
        &sub,
        &home,
        &InstallOpts {
            platform: Some(Platform::ClaudeCode),
            scope: Some(Scope::Project),
            db: None,
            command: Some(bin.clone()),
            git_hooks: true,
            prewarm: false,
        },
        &McpCommand::Explicit(bin),
        &Externals::with_path(None),
    )
    .expect("install in the submodule");

    let hook = module.join("hooks").join("post-commit");
    assert!(
        std::fs::read_to_string(&hook)
            .unwrap_or_default()
            .contains(HOOK_BEGIN),
        "the submodule's own hooks dir holds the block: {hook:?}"
    );
    assert!(
        !repo.join(".git").join("hooks").join("post-commit").exists(),
        "a submodule must not write into the superproject's hooks"
    );
}

/// `--version`, the `version` subcommand and the library function all print
/// the same crate version.
#[test]
fn version_prints_semver() {
    let expected = format!("mushroomdb {}", env!("CARGO_PKG_VERSION"));
    assert_eq!(version_string(), expected);

    let semver = version_string();
    let number = semver.strip_prefix("mushroomdb ").unwrap();
    let parts: Vec<&str> = number.split('.').collect();
    assert_eq!(parts.len(), 3, "major.minor.patch: {number}");
    for p in parts {
        assert!(
            p.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "each component starts with a digit: {number}"
        );
    }

    for flag in ["--version", "-V", "version"] {
        let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag} failed");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            expected,
            "{flag}"
        );
    }
}
