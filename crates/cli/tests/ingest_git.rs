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

fn commit(repo: &Path, author: &str, msg: &str, files: &[(&str, &str)]) {
    for (p, body) in files {
        let full = repo.join(p);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }
    commit_all(repo, author, msg);
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
    }
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
