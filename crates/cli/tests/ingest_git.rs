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

fn commit(repo: &Path, author: &str, msg: &str, files: &[(&str, &str)]) {
    for (p, body) in files {
        let full = repo.join(p);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }
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
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=alice",
            "-c",
            "user.email=alice@x.test",
            "commit",
            "-q",
            "-m",
            "drop model",
        ],
    );
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
}

#[test]
fn rename_keeps_history_and_moves_edges() {
    let repo = seed_repo();
    let db_dir = tmp("db");
    run_ingest_git(&db_dir, &opts(&repo)).unwrap();
    // `git mv` will not create the destination directory itself.
    std::fs::create_dir_all(repo.join("src/domain")).unwrap();
    git(&repo, &["mv", "src/model.rs", "src/domain/model.rs"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=alice",
            "-c",
            "user.email=alice@x.test",
            "commit",
            "-q",
            "-m",
            "move model",
        ],
    );
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
