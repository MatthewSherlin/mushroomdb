//! ingest-git builds a codebase graph from git history and keeps it current
//! across re-runs (adds, modifies, deletes, renames) so derived edges retract.
use cli::ingest_git::{run_ingest_git, IngestGitOpts};
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
        "mushroomdb-ingest-git-{name}-{}-{nanos}-{seq}",
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

/// Stage whatever is in the worktree (including `git rm` / `git mv` results)
/// and commit it.
fn commit_all(repo: &Path, author: &str, msg: &str) {
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "-c",
            &format!("user.name={author}"),
            "-c",
            &format!("user.email={author}@x.test"),
            "commit",
            "-q",
            "-m",
            msg,
        ],
    );
}

/// Like [`commit_all`] but with the author's email given explicitly, so a test
/// can commit as the same person under two addresses.
fn commit_all_as(repo: &Path, name: &str, email: &str, msg: &str) {
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "-c",
            &format!("user.name={name}"),
            "-c",
            &format!("user.email={email}"),
            "commit",
            "-q",
            "-m",
            msg,
        ],
    );
}

fn commit(repo: &Path, author: &str, msg: &str, files: &[(&str, &str)]) {
    write_files(repo, files);
    commit_all(repo, author, msg);
}

fn commit_as(repo: &Path, name: &str, email: &str, msg: &str, files: &[(&str, &str)]) {
    write_files(repo, files);
    commit_all_as(repo, name, email, msg);
}

fn write_files(repo: &Path, files: &[(&str, &str)]) {
    for (p, body) in files {
        let full = repo.join(p);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }
}

/// Absolute path of a program on the ambient PATH, for the shims below.
fn which(program: &str) -> String {
    let out = Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .unwrap();
    assert!(out.status.success(), "{program} is not on PATH");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Write `body` to `dir/name` and make it executable.
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    p
}

/// `git mv`, creating the destination directory first — git will not.
fn mv(repo: &Path, from: &str, to: &str) {
    if let Some(parent) = Path::new(to).parent() {
        std::fs::create_dir_all(repo.join(parent)).unwrap();
    }
    git(repo, &["mv", from, to]);
}

fn seed_repo() -> PathBuf {
    let repo = tmp("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(
        &repo,
        "alice",
        "init api and model",
        &[("src/api.rs", "a1"), ("src/model.rs", "m1")],
    );
    commit(
        &repo,
        "alice",
        "api+model again",
        &[("src/api.rs", "a2"), ("src/model.rs", "m2")],
    );
    commit(&repo, "bob", "docs only", &[("docs/readme.md", "r1")]);
    commit(
        &repo,
        "alice",
        "api+model third",
        &[("src/api.rs", "a3"), ("src/model.rs", "m3")],
    );
    repo
}

fn rev_parse_head(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn sync_marker(db_dir: &Path) -> String {
    let db = GraphDb::open(db_dir).unwrap();
    let sha = db
        .node_ref("__mushroomdb_git_sync__")
        .and_then(|n| n.prop("sha"));
    match sha {
        Some(core_api::Value::Str(s)) => s,
        other => panic!("no sync marker sha: {other:?}"),
    }
}

/// The marker must equal the head the walk was bounded by, and every commit it
/// can reach must already be a `Commit` node. A marker that ran ahead of the
/// walk would silently skip those commits on every later run.
fn assert_marker_covers_everything_ingested(db_dir: &Path, repo: &Path) {
    let marker = sync_marker(db_dir);
    let db = GraphDb::open(db_dir).unwrap();
    assert_eq!(
        marker,
        rev_parse_head(repo),
        "marker must be the head the walk ended on"
    );
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", &marker])
        .output()
        .unwrap();
    assert!(out.status.success());
    let listed = String::from_utf8(out.stdout).unwrap();
    let mut n = 0;
    for sha in listed.lines() {
        assert!(
            db.has_node(sha),
            "{sha} is reachable from the marker but was never ingested"
        );
        n += 1;
    }
    assert!(n > 0, "rev-list returned nothing");
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

/// `std::fs::canonicalize`, which is what `ingest-git` records as `GitSync.repo`
/// — on macOS the temp dir is a symlink, so the raw path never compares equal.
fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap()
}

#[test]
fn initial_ingest_builds_files_authors_and_co_change_edges() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    let r = run_ingest_git(&db_dir, &opts(&repo)).expect("ingest");
    assert_eq!((r.commits, r.files, r.authors), (4, 3, 2));
    assert!(!r.incremental);
    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("src/api.rs") && db.has_node("docs/readme.md"));
    // co-changed both ways, docs excluded (jaccard 0 with the src files)
    let n = db
        .neighbors("src/api.rs", "CO_CHANGED", Direction::Out)
        .unwrap();
    assert_eq!(n, vec!["src/model.rs".to_string()]);
    assert!(db
        .neighbors("docs/readme.md", "CO_CHANGED", Direction::Out)
        .unwrap()
        .is_empty());
    // ownership: alice authored 3 of 3 commits on api.rs
    assert_eq!(
        db.neighbors("src/api.rs", "TOP_AUTHOR", Direction::Out)
            .unwrap(),
        vec!["alice@x.test".to_string()]
    );
    // via-hop: alice KNOWS model.rs through api.rs (co-change overlap)
    assert!(db
        .neighbors("alice@x.test", "KNOWS", Direction::Out)
        .unwrap()
        .contains(&"src/model.rs".to_string()));
    // explain gives the rule and a score
    let ex = db.explain("src/api.rs", "src/model.rs").unwrap();
    assert_eq!(ex[0].rule, "co_changed");
    assert!(ex[0].weight.unwrap() > 0.7);
    assert!(db
        .fulltext_pairs()
        .contains(&("File".to_string(), "path".to_string())));
}

#[test]
fn rerun_is_incremental_and_delete_retracts_edges() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    // delete model.rs in a new commit
    std::fs::remove_file(repo.join("src/model.rs")).unwrap();
    commit_all(&repo, "alice", "drop model");
    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert!(r.incremental);
    assert_eq!((r.commits, r.deleted), (1, 1));
    let db = GraphDb::open(&db_dir).unwrap();
    assert!(
        !db.has_node("src/model.rs"),
        "deleted file node must be removed"
    );
    assert!(
        db.neighbors("src/api.rs", "CO_CHANGED", Direction::Out)
            .unwrap()
            .is_empty(),
        "co-change edge must retract"
    );
    assert!(!db
        .neighbors("alice@x.test", "KNOWS", Direction::Out)
        .unwrap()
        .contains(&"src/model.rs".to_string()));
    assert_eq!(
        db.query("MATCH (c:Commit) RETURN count(*) AS n", &Default::default())
            .unwrap()
            .get(0, "n"),
        Some(&core_api::Value::Int(5))
    );
    drop(db);
    assert_marker_covers_everything_ingested(&db_dir, &repo);
}

#[test]
fn rename_keeps_history_and_moves_edges() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    mv(&repo, "src/model.rs", "src/domain/model.rs");
    commit_all(&repo, "alice", "move model");
    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!(r.renamed, 1);
    let db = GraphDb::open(&db_dir).unwrap();
    assert!(!db.has_node("src/model.rs"));
    assert!(db.has_node("src/domain/model.rs"));
    assert_eq!(
        db.neighbors("src/api.rs", "CO_CHANGED", Direction::Out)
            .unwrap(),
        vec!["src/domain/model.rs".to_string()]
    );
    let n = db.node_ref("src/domain/model.rs").unwrap();
    assert_eq!(
        n.prop("n_commits"),
        Some(core_api::Value::Int(4)),
        "history follows the rename"
    );
    assert_eq!(
        n.prop("id"),
        Some(core_api::Value::Str("src/domain/model.rs".to_string())),
        "the id property follows the key"
    );
}

#[test]
fn exclude_patterns_skip_vendored_paths() {
    let repo = seed_repo();
    commit(
        &repo,
        "bob",
        "vendor blob",
        &[("lib/vendor/big.rs", "v"), ("lib/vendor/other.rs", "w")],
    );
    let db_dir = tmp("db");
    let mut o = opts(&repo);
    o.exclude = vec!["lib/vendor/".into(), "*.md".into()];
    let r = run_ingest_git(&db_dir, &o).unwrap();
    assert_eq!(r.files, 2, "only src/api.rs and src/model.rs remain");
}

#[test]
fn rerun_without_new_commits_is_a_noop() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    let before = GraphDb::open(&db_dir).unwrap().commit_seq();
    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!(r.commits, 0);
    assert_eq!(
        GraphDb::open(&db_dir).unwrap().commit_seq(),
        before,
        "no writes on a no-op rerun"
    );
}

/// A file renamed and then deleted inside one window must leave nothing behind.
/// Renaming into a path that does not survive the window would strand a node
/// under the new key with stale props, which the next run would then duplicate
/// (its `id` prop still names the old path).
#[test]
fn rename_then_delete_in_one_window_leaves_no_phantom_node() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    mv(&repo, "src/api.rs", "src/renamed.rs");
    commit_all(&repo, "alice", "move api");
    std::fs::remove_file(repo.join("src/renamed.rs")).unwrap();
    commit_all(&repo, "alice", "drop the moved api");

    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!((r.commits, r.renamed, r.deleted), (2, 0, 1));

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(!db.has_node("src/api.rs"), "the original path is gone");
    assert!(
        !db.has_node("src/renamed.rs"),
        "the rename destination must not survive its own deletion"
    );
    // Nothing still points at either path.
    assert!(db
        .neighbors("src/model.rs", "CO_CHANGED", Direction::Out)
        .unwrap()
        .is_empty());
    let touched = db
        .query(
            "MATCH (c:Commit)-[:TOUCHED]->(f:File) RETURN f.id AS id",
            &Default::default(),
        )
        .unwrap();
    for i in 0..touched.len() {
        let id = touched.get(i, "id").cloned();
        assert_ne!(id, Some(core_api::Value::Str("src/api.rs".into())));
        assert_ne!(id, Some(core_api::Value::Str("src/renamed.rs".into())));
    }
    // Exactly one File node per live path, and no id/key disagreement.
    let files = db
        .query("MATCH (f:File) RETURN f.id AS id", &Default::default())
        .unwrap();
    assert_eq!(files.len(), 2, "only model.rs and readme.md remain");

    // A further run must find nothing to do — no resurrected duplicate.
    let before = db.commit_seq();
    drop(db);
    let again = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!(again.commits, 0);
    assert_eq!(
        GraphDb::open(&db_dir).unwrap().commit_seq(),
        before,
        "the follow-up run writes nothing"
    );
}

/// Two renames of the same file in one window collapse to a single move, so the
/// node lands on the final path with its whole history rather than being
/// dropped and recreated at the intermediate name.
#[test]
fn chained_rename_in_one_window_collapses_to_one_move() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    mv(&repo, "src/model.rs", "src/model2.rs");
    commit_all(&repo, "alice", "first move");
    mv(&repo, "src/model2.rs", "src/domain/model.rs");
    commit_all(&repo, "alice", "second move");

    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!((r.commits, r.renamed, r.deleted), (2, 1, 0));

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(!db.has_node("src/model.rs"));
    assert!(
        !db.has_node("src/model2.rs"),
        "the intermediate path must not exist"
    );
    assert!(db.has_node("src/domain/model.rs"));
    let n = db.node_ref("src/domain/model.rs").unwrap();
    assert_eq!(
        n.prop("n_commits"),
        Some(core_api::Value::Int(5)),
        "3 original commits plus both moves"
    );
    assert_eq!(
        n.prop("id"),
        Some(core_api::Value::Str("src/domain/model.rs".to_string()))
    );
    assert_eq!(
        db.neighbors("src/api.rs", "CO_CHANGED", Direction::Out)
            .unwrap(),
        vec!["src/domain/model.rs".to_string()]
    );
    // The commit that touched the intermediate name points at the final node.
    let touched = db
        .neighbors("src/domain/model.rs", "TOUCHED", Direction::In)
        .unwrap();
    assert_eq!(touched.len(), 5, "every touching commit retargeted");
}

/// A file moved away and moved back inside one window collapses to no move at
/// all, rather than a rename of the node onto its own key.
#[test]
fn rename_that_swaps_back_in_one_window_is_not_a_move() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    mv(&repo, "src/model.rs", "src/model2.rs");
    commit_all(&repo, "alice", "move away");
    mv(&repo, "src/model2.rs", "src/model.rs");
    commit_all(&repo, "alice", "move back");

    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!((r.commits, r.renamed, r.deleted), (2, 0, 0));

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("src/model.rs"));
    assert!(!db.has_node("src/model2.rs"));
    let n = db.node_ref("src/model.rs").unwrap();
    assert_eq!(
        n.prop("n_commits"),
        Some(core_api::Value::Int(5)),
        "both moves still count as commits touching the file"
    );
    assert_eq!(
        n.prop("id"),
        Some(core_api::Value::Str("src/model.rs".to_string()))
    );
}

/// A rename whose destination is excluded is a delete: the old node goes and no
/// node appears under the vendored path.
#[test]
fn rename_into_excluded_path_deletes_the_node() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    let mut o = opts(&repo);
    o.exclude = vec!["vendor/".into()];
    run_ingest_git(&db_dir, &o).unwrap();
    mv(&repo, "src/model.rs", "vendor/model.rs");
    commit_all(&repo, "alice", "vendor the model");

    let r = run_ingest_git(&db_dir, &o).unwrap();
    assert_eq!((r.renamed, r.deleted), (0, 1));

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(!db.has_node("src/model.rs"));
    assert!(
        !db.has_node("vendor/model.rs"),
        "excluded destination gets no node"
    );
    assert!(db
        .neighbors("src/api.rs", "CO_CHANGED", Direction::Out)
        .unwrap()
        .is_empty());
}

/// A path freed by a delete and immediately claimed by a rename: the stale node
/// under that path is removed so `rename_node` has somewhere to land.
#[test]
fn rename_onto_a_just_deleted_path_replaces_the_old_node() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    std::fs::remove_file(repo.join("src/api.rs")).unwrap();
    commit_all(&repo, "alice", "drop api");
    mv(&repo, "docs/readme.md", "src/api.rs");
    commit_all(&repo, "bob", "readme takes the api path");

    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!((r.commits, r.renamed, r.deleted), (2, 1, 1));

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(!db.has_node("docs/readme.md"));
    assert!(db.has_node("src/api.rs"), "the moved node holds the path");
    let n = db.node_ref("src/api.rs").unwrap();
    assert_eq!(
        n.prop("n_commits"),
        Some(core_api::Value::Int(2)),
        "readme's own commit plus the move, not the old api.rs history"
    );
    assert_eq!(
        n.prop("id"),
        Some(core_api::Value::Str("src/api.rs".to_string()))
    );
    assert_eq!(
        n.prop("top_author_id"),
        Some(core_api::Value::Str("bob@x.test".to_string())),
        "ownership came across with the node, it is not the old api.rs owner"
    );
    let files = db
        .query("MATCH (f:File) RETURN f.id AS id", &Default::default())
        .unwrap();
    assert_eq!(files.len(), 2, "no duplicate under the reclaimed path");
}

/// Node keys are one namespace, so a repository file named `HEAD` must not
/// collide with the `GitSync` marker. If it did, the sha would land on the File
/// node, no marker would exist, and every run would re-ingest the whole history.
#[test]
fn a_repo_file_named_head_does_not_collide_with_the_sync_marker() {
    let repo = seed_repo();
    commit(&repo, "bob", "add a HEAD file", &[("HEAD", "not a ref")]);
    let db_dir = tmp("db");
    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!(r.files, 4);

    let db = GraphDb::open(&db_dir).unwrap();
    let head_file = db.node_ref("HEAD").expect("the repo file keeps its path");
    assert_eq!(head_file.label(), "File");
    assert_eq!(
        head_file.prop("path"),
        Some(core_api::Value::Str("HEAD".to_string()))
    );
    assert!(
        head_file.prop("sha").is_none(),
        "the sync sha must not be written onto the File node"
    );
    let marker = db
        .node_ref("__mushroomdb_git_sync__")
        .expect("sync marker exists under its own key");
    assert_eq!(marker.label(), "GitSync");
    assert_eq!(
        marker.prop("id"),
        Some(core_api::Value::Str("__mushroomdb_git_sync__".to_string()))
    );
    assert!(matches!(marker.prop("sha"), Some(core_api::Value::Str(s)) if s.len() == 40));
    drop(db);

    // The marker is found again, so the rerun is incremental and writes nothing.
    let before = GraphDb::open(&db_dir).unwrap().commit_seq();
    let again = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert!(again.incremental);
    assert_eq!(again.commits, 0);
    assert_eq!(GraphDb::open(&db_dir).unwrap().commit_seq(), before);
}

/// The sync marker is exactly `git rev-parse HEAD`, and a merge whose side
/// branch is dated years in the future does not disturb that or the incremental
/// resume. Merge commits themselves carry no `TOUCHED` edges.
#[test]
fn sync_marker_is_head_across_a_merge_with_skewed_dates() {
    let repo = seed_repo();
    // A side branch off the first commit, dated far in the future, merged back.
    git(&repo, &["checkout", "-q", "-b", "feat", "HEAD~3"]);
    let full = repo.join("src/side.rs");
    std::fs::write(&full, "s1").unwrap();
    git(&repo, &["add", "-A"]);
    let st = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "-c",
            "user.name=bob",
            "-c",
            "user.email=bob@x.test",
            "commit",
            "-q",
            "-m",
            "future side work",
        ])
        .env("GIT_AUTHOR_DATE", "2030-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2030-01-01T00:00:00Z")
        .status()
        .unwrap();
    assert!(st.success());
    git(&repo, &["checkout", "-q", "main"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=alice",
            "-c",
            "user.email=alice@x.test",
            "merge",
            "-q",
            "--no-ff",
            "feat",
            "-m",
            "merge the side branch",
        ],
    );

    let db_dir = tmp("db");
    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!(r.commits, 6, "4 seed + side commit + merge");

    let real_head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    assert_eq!(
        sync_marker(&db_dir),
        real_head,
        "the marker is exactly rev-parse HEAD"
    );
    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("src/side.rs"));
    assert!(
        db.neighbors(&real_head, "TOUCHED", Direction::Out)
            .unwrap()
            .is_empty(),
        "a merge commit reports no file changes"
    );
    drop(db);

    // The marker resumes correctly: nothing new, nothing written.
    let before = GraphDb::open(&db_dir).unwrap().commit_seq();
    let again = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!(again.commits, 0);
    assert_eq!(GraphDb::open(&db_dir).unwrap().commit_seq(), before);
}

/// Non-ASCII paths are stored as written. Without `core.quotePath=false` git
/// renders them octal-escaped and quoted, so the node key would be mangled and
/// no later run would match it.
#[test]
fn non_ascii_paths_are_stored_unescaped() {
    let repo = seed_repo();
    commit(
        &repo,
        "bob",
        "add an accented file",
        &[("src/café.rs", "c1")],
    );
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(
        db.has_node("src/café.rs"),
        "the real path is the key; keys present: {:?}",
        db.query("MATCH (f:File) RETURN f.id AS id", &Default::default())
            .unwrap()
            .len()
    );
    let n = db.node_ref("src/café.rs").unwrap();
    assert_eq!(
        n.prop("path"),
        Some(core_api::Value::Str("src/café.rs".to_string()))
    );
    assert_eq!(n.prop("ext"), Some(core_api::Value::Str("rs".to_string())));
}

/// An initialised repository with no commits reports zeros and writes nothing.
#[test]
fn empty_repository_reports_zeros_and_writes_nothing() {
    let repo = tmp("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    let db_dir = tmp("db");

    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!((r.commits, r.files, r.authors), (0, 0, 0));
    assert!(!r.incremental);
    assert!(r.rules_created.is_empty(), "no rules on an empty repo");

    let db = GraphDb::open(&db_dir).unwrap();
    assert_eq!(db.commit_seq(), 0, "nothing was written");
    assert_eq!(db.stats().nodes_live, 0);
    drop(db);

    // The first real commit still gets a full, non-incremental ingest.
    commit(&repo, "alice", "first", &[("src/api.rs", "a1")]);
    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!((r.commits, r.files, r.authors), (1, 1, 1));
    assert!(!r.incremental, "the empty run left no sync marker");
    assert!(r.rules_created.contains(&"co_changed".to_string()));
}

/// If the recorded sync head is not in the repository the run fails loudly
/// instead of replaying the whole history on top of what is already stored.
#[test]
fn missing_sync_head_errors_without_writing() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    let before = GraphDb::open(&db_dir).unwrap().commit_seq();

    // A different repository, whose history does not contain the recorded sha.
    let other = tmp("other");
    git(&other, &["init", "-q", "-b", "main"]);
    commit(&other, "carol", "unrelated work", &[("other.rs", "z")]);

    let err = run_ingest_git(&db_dir, &opts(&other)).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("sync head") && msg.contains("fresh database"),
        "error should name the problem and the way out, got: {msg}"
    );
    assert_eq!(
        GraphDb::open(&db_dir).unwrap().commit_seq(),
        before,
        "a failed run writes nothing"
    );
}

/// Ownership must be able to change across incremental runs. Per-author counts
/// are persisted on the `File` node, so a challenger's commits accumulate over
/// separate syncs instead of being credited to the incumbent on every reload.
///
/// alice opens the file with 3 commits, then bob adds 4 — one per incremental
/// run. Bob leads 4-3 and must own the file, and a full ingest of the same
/// repository into a fresh store must agree.
#[test]
fn ownership_flips_across_incremental_runs() {
    let repo = tmp("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    for i in 0..3 {
        commit(
            &repo,
            "alice",
            &format!("alice {i}"),
            &[("src/api.rs", &format!("a{i}"))],
        );
    }
    let db_dir = tmp("db");
    let first = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert!(!first.incremental);
    assert_eq!(
        GraphDb::open(&db_dir)
            .unwrap()
            .node_ref("src/api.rs")
            .unwrap()
            .prop("top_author_id"),
        Some(core_api::Value::Str("alice@x.test".to_string())),
        "alice owns the file after her three commits"
    );

    // Four separate incremental syncs, one bob commit each.
    for i in 0..4 {
        commit(
            &repo,
            "bob",
            &format!("bob {i}"),
            &[("src/api.rs", &format!("b{i}"))],
        );
        let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
        assert!(r.incremental, "run {i} should be incremental");
    }

    let incremental_top = GraphDb::open(&db_dir)
        .unwrap()
        .node_ref("src/api.rs")
        .unwrap()
        .prop("top_author_id");
    assert_eq!(
        incremental_top,
        Some(core_api::Value::Str("bob@x.test".to_string())),
        "bob has 4 commits to alice's 3, so ownership must move to bob"
    );

    // A fresh full ingest of the same repository is the oracle.
    let full_dir = tmp("db-full");
    run_ingest_git(&full_dir, &opts(&repo)).unwrap();
    let full_db = GraphDb::open(&full_dir).unwrap();
    let full_node = full_db.node_ref("src/api.rs").unwrap();
    assert_eq!(
        incremental_top,
        full_node.prop("top_author_id"),
        "incremental sync must agree with a full ingest of the same repo"
    );
    assert_eq!(full_node.prop("n_commits"), Some(core_api::Value::Int(7)));

    // TOP_AUTHOR is an auto-FK on top_author_id, so the edge must follow.
    let db = GraphDb::open(&db_dir).unwrap();
    assert_eq!(
        db.neighbors("src/api.rs", "TOP_AUTHOR", Direction::Out)
            .unwrap(),
        vec!["bob@x.test".to_string()],
    );
}

/// A parent repository with an initialised submodule checked out at
/// `vendor/lib`, plus the repository the submodule was cloned from.
fn seed_repo_with_submodule() -> PathBuf {
    let lib = tmp("lib");
    git(&lib, &["init", "-q", "-b", "main"]);
    commit(&lib, "carol", "lib init", &[("src/lib.rs", "l1")]);

    let app = tmp("app");
    git(&app, &["init", "-q", "-b", "main"]);
    commit(&app, "alice", "app init", &[("src/app.rs", "a1")]);
    git(
        &app,
        &[
            // A file:// submodule source is refused by default since git 2.38.
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            lib.to_str().unwrap(),
            "vendor/lib",
        ],
    );
    commit_all(&app, "alice", "add the vendored library");
    app
}

/// The `sha` on one unit's sync marker, or `None` when it has none. Opens its
/// own handle, so no other handle may be live when it is called.
fn marker(db_dir: &Path, key: &str) -> Option<String> {
    let db = GraphDb::open(db_dir).unwrap();
    match db.node_ref(key).and_then(|n| n.prop("sha")) {
        Some(core_api::Value::Str(s)) => Some(s),
        _ => None,
    }
}

fn run_cli(db_dir: &Path, repo: &Path, extra: &[&str], path: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("ingest-git")
        .arg(db_dir)
        .arg(repo)
        .args(extra)
        .env("PATH", path)
        .output()
        .unwrap()
}

/// One person, two addresses, one `.mailmap`: the graph must hold a single
/// `Author`, keyed by the canonical address, with the canonical name.
#[test]
fn mailmap_merges_two_emails_into_one_author() {
    let repo = tmp("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit_as(
        &repo,
        "Alice Example",
        "alice@x.test",
        "init api",
        &[
            ("src/api.rs", "a1"),
            (
                ".mailmap",
                "Alice Example <alice@x.test> <alice.old@x.test>\n",
            ),
        ],
    );
    commit_as(
        &repo,
        "alice",
        "alice.old@x.test",
        "api again, old address",
        &[("src/api.rs", "a2")],
    );

    let db_dir = tmp("db");
    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!(r.authors, 1, "the two addresses are one author");

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(
        !db.has_node("alice.old@x.test"),
        "the mapped-away address must not become an Author"
    );
    let a = db.node_ref("alice@x.test").expect("canonical author");
    assert_eq!(a.label(), "Author");
    assert_eq!(
        a.prop("name"),
        Some(core_api::Value::Str("Alice Example".to_string())),
        "the mailmap name wins over the raw commit name"
    );
    assert_eq!(
        db.node_ref("src/api.rs").unwrap().prop("top_author_id"),
        Some(core_api::Value::Str("alice@x.test".to_string())),
        "both commits count towards the canonical identity"
    );
}

/// With `--recurse-submodules` a submodule is its own unit: its file keys carry
/// the submodule path, and it resumes from its own `GitSync` marker.
#[test]
fn recurse_submodules_prefixes_keys_and_keeps_separate_sync_markers() {
    let app = seed_repo_with_submodule();
    let sub = app.join("vendor/lib");
    let db_dir = tmp("db");
    let mut o = opts(&app);
    o.recurse_submodules = true;

    let r = run_ingest_git(&db_dir, &o).unwrap();
    assert_eq!(r.submodules, 1);
    assert_eq!(
        (r.commits, r.files, r.authors),
        (3, 3, 2),
        "2 parent commits + 1 submodule commit; src/app.rs, .gitmodules, vendor/lib/src/lib.rs"
    );

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("src/app.rs"));
    assert!(
        db.has_node("vendor/lib/src/lib.rs"),
        "submodule files are keyed by their path in the parent"
    );
    assert!(
        !db.has_node("src/lib.rs"),
        "the unprefixed submodule path must not appear"
    );
    assert!(
        !db.has_node("vendor/lib"),
        "the gitlink itself is not a file"
    );
    drop(db);
    assert_eq!(
        marker(&db_dir, "__mushroomdb_git_sync__"),
        Some(rev_parse_head(&app))
    );
    assert_eq!(
        marker(&db_dir, "__mushroomdb_git_sync__:vendor/lib"),
        Some(rev_parse_head(&sub)),
        "the submodule resumes from its own marker"
    );

    // A commit inside the submodule only advances the submodule's marker.
    commit(&sub, "carol", "lib update", &[("src/lib.rs", "l2")]);
    let again = run_ingest_git(&db_dir, &o).unwrap();
    assert!(again.incremental);
    assert_eq!(again.commits, 1, "only the submodule moved");

    let db = GraphDb::open(&db_dir).unwrap();
    assert_eq!(
        db.node_ref("vendor/lib/src/lib.rs")
            .unwrap()
            .prop("n_commits"),
        Some(core_api::Value::Int(2)),
        "the submodule file keeps its cumulative history"
    );
    drop(db);
    assert_eq!(
        marker(&db_dir, "__mushroomdb_git_sync__"),
        Some(rev_parse_head(&app))
    );
    assert_eq!(
        marker(&db_dir, "__mushroomdb_git_sync__:vendor/lib"),
        Some(rev_parse_head(&sub))
    );
}

/// Without the flag the submodule is not walked at all, and the gitlink entry
/// the parent records for it does not become a `File`.
#[test]
fn without_recurse_the_gitlink_is_ignored() {
    let app = seed_repo_with_submodule();
    let db_dir = tmp("db");
    let r = run_ingest_git(&db_dir, &opts(&app)).unwrap();
    assert_eq!(r.submodules, 0);
    assert_eq!(
        (r.commits, r.files),
        (2, 2),
        "only the parent's commits, and only src/app.rs and .gitmodules"
    );

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("src/app.rs") && db.has_node(".gitmodules"));
    assert!(
        !db.has_node("vendor/lib"),
        "the gitlink is a submodule pointer, not a file"
    );
    assert!(!db.has_node("vendor/lib/src/lib.rs"));
    assert!(
        db.node_ref("__mushroomdb_git_sync__:vendor/lib").is_none(),
        "no marker for a submodule that was never walked"
    );
}

/// `--prs` without `gh` on PATH prints one warning and ingests everything else.
#[test]
fn prs_without_gh_is_a_clean_skip() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    // A PATH holding git and nothing else, so `gh` cannot be found.
    let shim = tmp("shim");
    write_script(
        &shim,
        "git",
        &format!("#!/bin/sh\nexec {} \"$@\"\n", which("git")),
    );

    let out = run_cli(&db_dir, &repo, &["--prs"], shim.to_str().unwrap());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a missing gh is not an error: {stderr}"
    );
    assert_eq!(
        stderr.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "exactly one warning line, got: {stderr}"
    );
    assert!(stderr.contains("gh"), "the warning names gh: {stderr}");

    let db = GraphDb::open(&db_dir).unwrap();
    assert!(db.has_node("src/api.rs"), "the git ingest still ran");
    assert_eq!(
        db.query("MATCH (p:PR) RETURN p.id AS id", &Default::default())
            .unwrap()
            .len(),
        0,
        "no PR nodes without gh"
    );
    assert!(!db
        .fulltext_pairs()
        .contains(&("PR".to_string(), "title".to_string())));
}

/// Merged pull requests become `PR` nodes linked to the commit that carried
/// them: the merge commit by sha, and a squash merge by its `(#N)` subject.
#[test]
fn prs_from_fake_gh_link_merge_and_squash_commits() {
    let repo = tmp("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, "alice", "init api", &[("src/api.rs", "a1")]);
    git(&repo, &["checkout", "-q", "-b", "feat"]);
    commit(&repo, "bob", "feature work", &[("src/feat.rs", "f1")]);
    git(&repo, &["checkout", "-q", "main"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=alice",
            "-c",
            "user.email=alice@x.test",
            "merge",
            "-q",
            "--no-ff",
            "feat",
            "-m",
            "Merge pull request #7 from example/feat",
        ],
    );
    let merge_sha = rev_parse_head(&repo);
    commit(&repo, "bob", "add widget (#42)", &[("src/widget.rs", "w1")]);
    let squash_sha = rev_parse_head(&repo);

    // A fake `gh` first on PATH, printing a fixed listing.
    let fake = tmp("gh");
    let json = fake.join("prs.json");
    std::fs::write(
        &json,
        format!(
            r#"[{{"number":7,"title":"Add the feature","url":"https://example.test/pr/7",
  "mergedAt":"2026-01-02T00:00:00Z","mergeCommit":{{"oid":"{merge_sha}"}},
  "author":{{"login":"bobby"}}}},
 {{"number":42,"title":"Add a widget","url":"https://example.test/pr/42",
  "mergedAt":"2026-01-03T00:00:00Z","mergeCommit":null,"author":{{"login":"alicia"}}}}]"#
        ),
    )
    .unwrap();
    write_script(
        &fake,
        "gh",
        &format!("#!/bin/sh\ncat {}\n", json.to_str().unwrap()),
    );
    let path = format!(
        "{}:{}",
        fake.to_str().unwrap(),
        std::env::var("PATH").unwrap_or_default()
    );

    let db_dir = tmp("db");
    let out = run_cli(&db_dir, &repo, &["--prs"], &path);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let db = GraphDb::open(&db_dir).unwrap();
    let pr = db.node_ref("pr:7").expect("pr:7 exists");
    assert_eq!(pr.label(), "PR");
    assert_eq!(pr.prop("number"), Some(core_api::Value::Int(7)));
    assert_eq!(
        pr.prop("title"),
        Some(core_api::Value::Str("Add the feature".to_string()))
    );
    assert_eq!(
        pr.prop("url"),
        Some(core_api::Value::Str(
            "https://example.test/pr/7".to_string()
        ))
    );
    assert_eq!(
        pr.prop("merged_at"),
        Some(core_api::Value::Str("2026-01-02T00:00:00Z".to_string()))
    );
    assert_eq!(
        pr.prop("author_login"),
        Some(core_api::Value::Str("bobby".to_string()))
    );

    // Merge commit: linked by sha.
    assert_eq!(
        db.neighbors("pr:7", "MERGED_AS", Direction::Out).unwrap(),
        vec![merge_sha.clone()]
    );
    assert_eq!(
        db.node_ref(&merge_sha).unwrap().prop("pr_id"),
        Some(core_api::Value::Str("pr:7".to_string()))
    );
    assert_eq!(
        db.neighbors(&merge_sha, "PR", Direction::Out).unwrap(),
        vec!["pr:7".to_string()],
        "the auto-FK on pr_id derives the Commit -> PR edge"
    );

    // Squash merge: linked by the `(#42)` subject.
    assert_eq!(
        db.neighbors("pr:42", "MERGED_AS", Direction::Out).unwrap(),
        vec![squash_sha.clone()]
    );
    assert_eq!(
        db.node_ref(&squash_sha).unwrap().prop("pr_id"),
        Some(core_api::Value::Str("pr:42".to_string()))
    );

    // A commit belonging to no pull request stays unlinked.
    let unrelated = db
        .query(
            "MATCH (c:Commit) WHERE c.message = 'init api' RETURN c.pr_id AS pr",
            &Default::default(),
        )
        .unwrap();
    assert_eq!(unrelated.get(0, "pr"), None);

    assert!(db
        .fulltext_pairs()
        .contains(&("PR".to_string(), "title".to_string())));
}

/// The sync marker records where the repository is and how it was ingested, so
/// a later run can repeat the same ingest without being told again.
#[test]
fn gitsync_records_repo_and_flags() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    let mut o = opts(&repo);
    o.recurse_submodules = true;
    o.prs = false;
    o.structure = false;
    o.docs = true;
    run_ingest_git(&db_dir, &o).unwrap();

    let db = GraphDb::open(&db_dir).unwrap();
    let m = db.node_ref("__mushroomdb_git_sync__").unwrap();
    assert_eq!(m.label(), "GitSync");
    assert_eq!(
        m.prop("repo"),
        Some(core_api::Value::Str(
            canonical(&repo).to_string_lossy().to_string()
        )),
        "the repository path is absolute"
    );
    assert_eq!(m.prop("recurse"), Some(core_api::Value::Bool(true)));
    assert_eq!(m.prop("prs"), Some(core_api::Value::Bool(false)));
    assert_eq!(m.prop("structure"), Some(core_api::Value::Bool(false)));
    assert_eq!(m.prop("docs"), Some(core_api::Value::Bool(true)));
    drop(db);

    // Re-running with the same flags and no new commits still writes nothing.
    let before = GraphDb::open(&db_dir).unwrap().commit_seq();
    run_ingest_git(&db_dir, &o).unwrap();
    assert_eq!(GraphDb::open(&db_dir).unwrap().commit_seq(), before);

    // Changing a flag updates the marker even with no new commits.
    o.structure = true;
    run_ingest_git(&db_dir, &o).unwrap();
    assert_eq!(
        GraphDb::open(&db_dir)
            .unwrap()
            .node_ref("__mushroomdb_git_sync__")
            .unwrap()
            .prop("structure"),
        Some(core_api::Value::Bool(true))
    );
}

/// `--ensure-gitignore` adds the database directory to the repository's
/// `.gitignore` once, and leaves it alone on every later run.
#[test]
fn ensure_gitignore_is_idempotent() {
    let repo = seed_repo();
    std::fs::write(repo.join(".gitignore"), "target").unwrap(); // no trailing newline
    let db_dir = repo.join("mushroom-memory");
    let mut o = opts(&repo);
    o.ensure_gitignore = true;

    let first = run_ingest_git(&db_dir, &o).unwrap();
    assert!(first.gitignore_added);
    assert_eq!(
        std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
        "target\nmushroom-memory/\n"
    );

    let again = run_ingest_git(&db_dir, &o).unwrap();
    assert!(!again.gitignore_added, "the line is already there");
    assert_eq!(
        std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
        "target\nmushroom-memory/\n",
        "a second run must not append a duplicate"
    );

    // A database outside the repository is not the repository's business.
    let other = seed_repo();
    let outside = tmp("db-outside");
    let mut oo = opts(&other);
    oo.ensure_gitignore = true;
    let r = run_ingest_git(&outside, &oo).unwrap();
    assert!(!r.gitignore_added);
    assert!(
        !other.join(".gitignore").exists(),
        "no .gitignore is created for a database stored elsewhere"
    );
}

/// The store takes a cross-process write lock, so a second writer has to be
/// told to retry rather than failing obscurely or corrupting the WAL.
#[test]
fn a_busy_store_exits_three_with_a_retry_message() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    // A plain read-write handle holds the lock for as long as it lives.
    let holder = GraphDb::open(&db_dir).unwrap();

    let out = run_cli(
        &db_dir,
        &repo,
        &[],
        &std::env::var("PATH").unwrap_or_default(),
    );
    assert_eq!(out.status.code(), Some(3), "Busy is exit code 3");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another mushroomdb process is writing; retry"),
        "got: {stderr}"
    );
    assert_eq!(holder.commit_seq(), 0, "the busy run wrote nothing");
    drop(holder);

    // Once the lock is free the same ingest succeeds.
    let r = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert_eq!(r.commits, 4);
}

/// An incremental run costs work proportional to what the commit touched, not
/// to how big the repository is.
///
/// The regression this pins: a run that picked up one commit over eight files
/// reported `397 file(s)` and wrote one WAL frame per property per file, so
/// every later open replayed frames for files nothing had touched. Both halves
/// are asserted — the reported count, and the number of commits the run appends.
#[test]
fn an_incremental_run_writes_only_what_the_commit_touched() {
    let repo = tmp("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    // Twenty files, so "proportional to the repo" and "proportional to the
    // commit" are far enough apart to tell apart.
    let bodies: Vec<(String, String)> = (0..20)
        .map(|i| (format!("src/f{i}.rs"), format!("pub fn f{i}() {{}}\n")))
        .collect();
    write_files(
        &repo,
        &bodies
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_str()))
            .collect::<Vec<_>>(),
    );
    commit_all(&repo, "alice", "twenty files");

    let db_dir = tmp("db");
    let full = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert!(!full.incremental);
    assert_eq!(full.files, 20, "the full run does write every file");

    let before = core_api::wal_commit_count_at(&db_dir).unwrap();

    // One commit, one file.
    commit(
        &repo,
        "alice",
        "touch one",
        &[("src/f3.rs", "pub fn f3() { let x = 1; }\n")],
    );
    let inc = run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    assert!(inc.incremental);
    assert_eq!(
        inc.files, 1,
        "an incremental run reports the files it wrote, not every file it knows"
    );

    let delta = core_api::wal_commit_count_at(&db_dir).unwrap() - before;
    assert!(
        delta <= 8,
        "one commit over one file appended {delta} WAL commits; \
         a run must not append work proportional to the repository"
    );
}
