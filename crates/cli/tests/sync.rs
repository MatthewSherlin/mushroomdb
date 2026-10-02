//! What is left of the hook surface once `sync` and `touch` went in 0.7: the
//! snapshot a full `ingest-git` takes, the `recall` hook body's silence at the
//! process boundary, `--auto` database discovery, where the cleanup of a 0.6
//! install's git hooks looks for them, and the version string.
use cli::ingest_git::{run_ingest_git, IngestGitOpts};
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

// ── snapshots ───────────────────────────────────────────────────────────────

fn wal_len(db_dir: &Path) -> u64 {
    std::fs::metadata(db_dir.join("wal.bin")).map_or(0, |m| m.len())
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
        !db.node_history("src/net.rs").unwrap().items.is_empty(),
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

// ── the recall hook at the process boundary ─────────────────────────────────

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

/// The prompt hook's body, held to its contract at the process boundary.
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
/// `--auto` into `.mcp.json` and the settings hooks; those
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

/// The `post-commit` block a 0.6 install wrote from a linked worktree, as
/// 0.6.12 wrote it with an explicit `--command` and `--auto`.
fn block_0_6(bin: &str) -> String {
    format!(
        "#!/bin/sh\n\n# >>> mushroomdb >>>\n( '{bin}' sync --auto >/dev/null 2>&1 & )\n# <<< mushroomdb <<<\n"
    )
}

/// `install` run from inside a linked worktree takes a 0.6 `sync` block out
/// of where git actually ran it: the repository's **common** hooks directory.
///
/// A linked worktree's gitdir is `<main>/.git/worktrees/<name>`, and that is
/// what the `gitdir:` link in its `.git` file points at — but git resolves
/// hooks through the common dir, so a cleanup that looked in the worktree's
/// own gitdir would find nothing and leave the block running on every commit.
/// `doctor` has to read the same directory, or it could not report the block
/// the cleanup is for.
#[test]
fn install_from_a_worktree_cleans_the_common_dir_hooks() {
    use cli::doctor::{run_doctor_with, DoctorOpts};
    use cli::install::{
        run_install_with, Delivery, Externals, InstallOpts, McpCommand, Platform, Scope,
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

    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));
    let common_hook = repo.join(".git").join("hooks").join("post-commit");
    std::fs::write(&common_hook, block_0_6(&bin.to_string_lossy())).unwrap();

    let home = tmp("wt-hooks-home");
    let install_opts = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::Project),
        db: None, // `--auto`: each checkout resolves its own store
        command: Some(bin.clone()),
        prewarm: false,
        delivery: Delivery::Both,
        always_load: false,
    };
    let doctor = |dir: &Path| {
        run_doctor_with(
            dir,
            &home,
            &DoctorOpts {
                platform: Some(Platform::ClaudeCode),
                scope: Some(Scope::Project),
            },
            &Externals::with_path(None),
        )
        .expect("doctor")
        .output
    };

    // Install once so doctor has a config to read, then put the 0.6 block
    // back as if the install had been 0.6's: doctor must find it where git
    // runs it.
    run_install_with(
        &wt,
        &home,
        &install_opts,
        &McpCommand::Explicit(bin.clone()),
        &Externals::with_path(None),
    )
    .expect("install from the worktree");
    assert!(
        !common_hook.exists(),
        "the first install already cleaned it"
    );
    std::fs::write(&common_hook, block_0_6(&bin.to_string_lossy())).unwrap();
    let report = doctor(&wt);
    let git_line = report
        .lines()
        .find(|l| l.contains("git-hooks"))
        .unwrap_or_else(|| panic!("no git-hooks line in:\n{report}"));
    assert!(git_line.starts_with("warn"), "{git_line}");
    assert!(
        git_line.contains(&common_hook.display().to_string()),
        "doctor reports the file in the common hooks dir: {git_line}"
    );

    let summary = run_install_with(
        &wt,
        &home,
        &install_opts,
        &McpCommand::Explicit(bin),
        &Externals::with_path(None),
    )
    .expect("install from the worktree");
    assert!(
        !common_hook.exists(),
        "the block was the whole file, so the file goes:\n{summary}"
    );
    assert!(
        summary.contains(&common_hook.display().to_string()),
        "the summary names the file it cleaned:\n{summary}"
    );
    let wt_gitdir = repo.join(".git").join("worktrees").join("feature");
    assert!(
        !wt_gitdir.join("hooks").exists(),
        "nothing may be written to the worktree's own gitdir"
    );
    assert!(!doctor(&wt).contains("git-hooks"), "nothing left to report");
}

/// A submodule keeps its own hooks: its gitdir has no `commondir`, and git
/// runs `.git/modules/<path>/hooks` for it — so that is where a 0.6 install
/// in a submodule put its block, and where the cleanup has to look. The
/// superproject's hooks are not the submodule's to touch.
///
/// The two shapes are told apart by that one file, so this is the other half
/// of [`install_from_a_worktree_cleans_the_common_dir_hooks`]. The gitdir
/// link is written by hand rather than by `git submodule add`, which needs a
/// clonable origin and is blocked over local paths by default on current git.
#[test]
fn install_in_a_submodule_cleans_the_submodule_hooks() {
    use cli::install::{
        run_install_with, Delivery, Externals, InstallOpts, McpCommand, Platform, Scope,
    };

    let repo = tmp("sub-hooks-main");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, "first", &[("src/lib.rs", LIB_RS)]);

    // Exactly the shape git leaves for a submodule: a gitdir link with no
    // `commondir` beside it.
    let module = repo.join(".git").join("modules").join("vendor").join("sub");
    std::fs::create_dir_all(module.join("hooks")).unwrap();
    let sub = repo.join("vendor").join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(".git"), format!("gitdir: {}\n", module.display())).unwrap();
    assert!(
        !module.join("commondir").exists(),
        "a submodule's gitdir has no commondir — that is the discriminator"
    );

    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));
    let block = block_0_6(&bin.to_string_lossy());
    let sub_hook = module.join("hooks").join("post-commit");
    let super_hook = repo.join(".git").join("hooks").join("post-commit");
    std::fs::write(&sub_hook, &block).unwrap();
    std::fs::write(&super_hook, &block).unwrap();

    let home = tmp("sub-hooks-home");
    run_install_with(
        &sub,
        &home,
        &InstallOpts {
            platform: Some(Platform::ClaudeCode),
            scope: Some(Scope::Project),
            db: None,
            command: Some(bin.clone()),
            prewarm: false,
            delivery: Delivery::Both,
            always_load: false,
        },
        &McpCommand::Explicit(bin),
        &Externals::with_path(None),
    )
    .expect("install in the submodule");

    assert!(!sub_hook.exists(), "the submodule's own block is cleaned");
    assert_eq!(
        std::fs::read_to_string(&super_hook).unwrap(),
        block,
        "a submodule must not edit the superproject's hooks"
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
