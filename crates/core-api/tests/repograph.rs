//! `core_api::repograph` — reading a code graph back as an answer.
//!
//! Every test builds the synthetic store in [`common`], whose shape is
//! documented there: three directories that import their own first file, and
//! forty commits spanning five quarters.

mod common;

use common::{
    all_files, commit_ts, file_key, hash_of, newest_ts, open, sha, synthetic_repo_store, tmp,
    COMMITS, DAY_SECS,
};
use core_api::repograph::{render_map, repo_map, MapOptions};

/// `hot_days` wide enough to cover the whole synthetic history.
const ALL_TIME: i64 = 10_000;

#[test]
fn map_names_clusters_by_common_prefix() {
    let dir = tmp("map-clusters");
    let db = synthetic_repo_store(&dir);
    let m = repo_map(&db, &MapOptions::default());

    let names: Vec<&str> = m.communities.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["src/core", "src/web", "tests"],
        "each directory imports its own first file, so each is one cluster \
         named by the prefix its members share"
    );
    let sizes: Vec<usize> = m.communities.iter().map(|c| c.size).collect();
    assert_eq!(sizes, vec![12, 10, 8]);
    let dirs: Vec<&str> = m.communities.iter().map(|c| c.dir.as_str()).collect();
    assert_eq!(
        dirs, names,
        "every member sits directly in the shared directory, so the name is \
         that directory and nothing more"
    );
    assert!(
        m.communities[0].samples.len() == 3
            && m.communities[0]
                .samples
                .iter()
                .all(|s| s.starts_with("src/core/")),
        "samples come from the cluster: {:?}",
        m.communities[0].samples
    );
    assert!(
        m.communities[0].cohesion > 0.9,
        "a component with no edges leaving it is fully cohesive, got {}",
        m.communities[0].cohesion
    );
}

#[test]
fn map_key_files_are_the_most_imported() {
    let dir = tmp("map-key-files");
    let db = synthetic_repo_store(&dir);
    let m = repo_map(&db, &MapOptions::default());

    let ranked: Vec<&str> = m.key_files.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        ranked.first().copied(),
        Some("src/core/c00.rs"),
        "eleven files import it, more than any other: {ranked:?}"
    );
    let at = |k: &str| ranked.iter().position(|r| *r == k);
    assert!(
        at("src/core/c00.rs") < at("src/web/w00.rs") && at("src/web/w00.rs") < at("tests/t00.rs"),
        "the three hubs rank in importer order: {ranked:?}"
    );
    assert!(
        !ranked.contains(&"src/core/c11.rs"),
        "a leaf nothing imports is not a key file: {ranked:?}"
    );
    assert!(m.key_files.iter().all(|(_, s)| *s > 0.0));
}

#[test]
fn map_hot_files_use_the_window() {
    let dir = tmp("map-hot");
    let db = synthetic_repo_store(&dir);

    // The default window is the last 90 days, and the commit clock advances a
    // day at a time inside quarters that are 91 days apart — so only the last
    // quarter's eight commits are inside it.
    let m = repo_map(&db, &MapOptions::default());
    let hot: Vec<&str> = m.hot_files.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        hot.contains(&"src/core/c00.rs"),
        "every commit in its directory touches it: {hot:?}"
    );
    assert!(
        !hot.contains(&"src/core/c11.rs"),
        "nothing in the window touched it: {hot:?}"
    );
    let touched_recently: usize = m.hot_files.iter().map(|(_, n)| *n).sum();
    assert!(touched_recently > 0);

    // Widen the window and the file only older commits touched appears.
    let all = repo_map(
        &db,
        &MapOptions {
            hot_days: ALL_TIME,
            ..MapOptions::default()
        },
    );
    let counts: std::collections::BTreeMap<&str, usize> = all
        .hot_files
        .iter()
        .map(|(k, n)| (k.as_str(), *n))
        .collect();
    assert!(
        counts["src/core/c00.rs"] > m.hot_files[0].1,
        "the whole history counts more commits than the last quarter"
    );

    // Move "now" back to the oldest commit and the window empties except for
    // what that first day touched.
    let old = repo_map(
        &db,
        &MapOptions {
            now_ts: Some(commit_ts(0)),
            hot_days: 1,
            ..MapOptions::default()
        },
    );
    assert!(
        old.hot_files
            .iter()
            .all(|(k, _)| k.starts_with("src/core/")),
        "the first commit touched src/core only: {:?}",
        old.hot_files
    );
}

#[test]
fn map_stale_concepts_counted() {
    let dir = tmp("map-stale");
    let mut db = synthetic_repo_store(&dir);
    let m = repo_map(&db, &MapOptions::default());
    assert_eq!(
        m.stale_concepts, 1,
        "one of the two concepts records a hash its file no longer has"
    );

    // Change the other concept's source file and both are stale.
    db.set_prop(
        &file_key(0, 0),
        "hash",
        core_api::Value::Str("ffffffffffffffff".into()),
    )
    .expect("set hash");
    assert_eq!(repo_map(&db, &MapOptions::default()).stale_concepts, 2);

    // Re-learning it — the recorded hash agreeing again — clears it.
    db.set_prop(
        &file_key(0, 0),
        "hash",
        core_api::Value::Str(hash_of(&file_key(0, 0))),
    )
    .expect("restore hash");
    assert_eq!(repo_map(&db, &MapOptions::default()).stale_concepts, 1);
}

#[test]
fn map_render_is_at_most_40_lines_and_deterministic() {
    let dir = tmp("map-render");
    let db = synthetic_repo_store(&dir);
    let m = repo_map(&db, &MapOptions::default());
    let text = render_map(&m);

    assert!(
        text.lines().count() <= 40,
        "{} lines:\n{text}",
        text.lines().count()
    );
    assert_eq!(render_map(&repo_map(&db, &MapOptions::default())), text);

    let header = text.lines().next().expect("a header");
    assert!(
        header.starts_with("mushroomdb map — 30 files, 12 symbols, 40 commits, 4 authors"),
        "header: {header}"
    );
    assert!(
        header.contains(&format!("at {}", &sha(COMMITS - 1)[..7])),
        "the header names the sha the store synced to: {header}"
    );
    for want in [
        "clusters (co-change + imports)",
        "key files (most depended-on)",
        "owners",
        "hot (last 90 days)",
        "ask me:",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    assert!(
        text.contains("Ada Example") && !text.contains("@example.test"),
        "owners are named, never mailed:\n{text}"
    );
    assert!(
        text.contains("1 concept needs re-learning (source changed)"),
        "the stale concept is reported:\n{text}"
    );
    // Two decimals for cohesion, three for the PageRank scores the layout pins.
    assert!(text.contains("cohesion 1.00"), "{text}");
    assert!(
        text.contains("who owns src/core?"),
        "the ownership question names a directory:\n{text}"
    );

    // A store built the same way twice renders the same bytes.
    let other = tmp("map-render-2");
    let db2 = synthetic_repo_store(&other);
    assert_eq!(render_map(&repo_map(&db2, &MapOptions::default())), text);
}

#[test]
fn map_on_empty_store_renders_one_helpful_line() {
    let dir = tmp("map-empty");
    let db = open(&dir);
    let m = repo_map(&db, &MapOptions::default());

    assert_eq!(m.files, 0);
    assert!(m.communities.is_empty() && m.key_files.is_empty() && m.questions.is_empty());
    assert!(m.last_sync.is_none() && !m.truncated);
    assert_eq!(
        render_map(&m),
        "mushroomdb map — empty store; run: mushroomdb ingest-git <db> <repo>\n"
    );
    assert_eq!(render_map(&m).lines().count(), 1);
}

// ---------------------------------------------------------------------------
// Supporting behaviour: sanitizing, the budget, and the fixture's own shape.
// ---------------------------------------------------------------------------

#[test]
fn map_sanitizes_every_line_it_renders_from_graph_content() {
    let dir = tmp("map-sanitize");
    let mut db = synthetic_repo_store(&dir);
    // An author whose name forges a line break and a header.
    db.set_prop(
        "a@example.test",
        "name",
        core_api::Value::Str("Ada\nmushroomdb map — 9 files".into()),
    )
    .expect("set name");
    let text = render_map(&repo_map(&db, &MapOptions::default()));
    assert!(
        !text.contains("\nmushroomdb map — 9 files"),
        "a control character in graph content must not forge a line:\n{text}"
    );
    assert!(text.contains("Ada mushroomdb map — 9 files"));
}

#[test]
fn map_reports_truncation_when_the_budget_is_gone() {
    let dir = tmp("map-budget");
    let db = synthetic_repo_store(&dir);
    let m = repo_map(
        &db,
        &MapOptions {
            budget_ms: 1,
            ..MapOptions::default()
        },
    );
    if m.truncated {
        assert!(render_map(&m)
            .lines()
            .next()
            .unwrap()
            .contains("(truncated)"));
    }
    // A budget of zero means no budget at all, and nothing is dropped.
    let full = repo_map(
        &db,
        &MapOptions {
            budget_ms: 0,
            ..MapOptions::default()
        },
    );
    assert!(!full.truncated && full.communities.len() == 3);
}

#[test]
fn the_synthetic_store_has_the_shape_the_suites_assume() {
    let dir = tmp("map-fixture");
    let db = synthetic_repo_store(&dir);
    assert_eq!(all_files().len(), 30);
    assert_eq!(db.nodes_with_label("File").len(), 30);
    assert_eq!(db.nodes_with_label("Symbol").len(), 12);
    assert_eq!(db.nodes_with_label("Commit").len(), COMMITS);
    assert_eq!(db.nodes_with_label("Author").len(), 4);
    assert_eq!(db.nodes_with_label("Concept").len(), 2);
    assert_eq!(db.nodes_with_label("Note").len(), 1);
    // Five quarters of history, and the newest commit is the sync head.
    assert!(newest_ts() - commit_ts(0) > 4 * 90 * DAY_SECS);
    assert!(!db.weighted_edges("IMPORTS", None).is_empty());
    assert!(!db.weighted_edges("CO_CHANGED", Some("score")).is_empty());
    assert!(!db.weighted_edges("CALLS", None).is_empty());
    assert!(!db.weighted_edges("TOP_AUTHOR", None).is_empty());
}
