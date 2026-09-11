//! `core_api::repograph` — reading a code graph back as an answer.
//!
//! Every test builds the synthetic store in [`common`], whose shape is
//! documented there: three directories that import their own first file, and
//! forty commits spanning five quarters.

mod common;

use common::{
    all_files, commit_author, commit_ts, doc_key, doc_mentions, file_key, hash_of, newest_ts, open,
    sha, synthetic_repo_store, tmp, touched, COMMITS, DAY_SECS, DOC_HEADING, DOC_HEADINGS,
    SYNCED_AT,
};
use core_api::repograph::{
    brief, context, context_with, explore, identifier_terms, impact, owners, recall_digest,
    remember, render_brief, render_context, render_explore, render_impact, render_map,
    render_owners, render_why, repo_map, shortest_path, stale_concepts, why, BriefOptions,
    ContextOptions, ContextReport, Depth, ImpactOptions, MapOptions, RememberInput, Target,
    DEFAULT_EXPLORE_BYTES, MAX_OUTPUT_BYTES, MAX_QUERY_TERMS, UNTRUSTED_FRAMING,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `hot_days` wide enough to cover the whole synthetic history.
const ALL_TIME: i64 = 10_000;

/// Options with the clock pinned twelve minutes after the fixture's sync, so
/// the whole digest — the sync age included — is fixed. Twelve minutes is far
/// short of the 90-day window, so which files count as hot is unchanged.
fn pinned() -> MapOptions {
    MapOptions {
        now_ts: Some(SYNCED_AT + 12 * 60),
        ..MapOptions::default()
    }
}

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

    // A source file that has gone counts as changed: whatever the concept
    // described is certainly not there any more.
    db.delete_node(&file_key(0, 0)).expect("delete the source");
    assert_eq!(repo_map(&db, &MapOptions::default()).stale_concepts, 2);

    // So do lists that do not pair up — a second source with no second hash
    // has nothing vouching for it, however well the first one checks out.
    db.set_prop(
        "concept:startup",
        "source_files",
        core_api::Value::List(vec![
            core_api::Value::Str(file_key(0, 1)),
            core_api::Value::Str(file_key(0, 2)),
        ]),
    )
    .expect("two sources");
    db.set_prop(
        "concept:startup",
        "source_hashes",
        core_api::Value::List(vec![core_api::Value::Str(hash_of(&file_key(0, 1)))]),
    )
    .expect("one hash");
    assert_eq!(
        repo_map(&db, &MapOptions::default()).stale_concepts,
        2,
        "the unpaired source keeps the concept stale even though the paired one matches"
    );
}

#[test]
fn map_render_is_at_most_40_lines_and_deterministic() {
    let dir = tmp("map-render");
    let db = synthetic_repo_store(&dir);
    let m = repo_map(&db, &pinned());
    let text = render_map(&m);

    assert!(
        text.lines().count() <= 40,
        "{} lines:\n{text}",
        text.lines().count()
    );
    assert_eq!(render_map(&repo_map(&db, &pinned())), text);

    let header = text.lines().next().expect("a header");
    assert!(
        header.starts_with("mushroomdb map — 30 files, 12 symbols, 40 commits, 4 authors"),
        "header: {header}"
    );
    assert!(
        header.ends_with(&format!("· synced 12m ago at {}", &sha(COMMITS - 1)[..7])),
        "the header dates the sync and names its sha: {header}"
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
    // Every float renders at two decimals, cohesion and PageRank alike.
    assert!(text.contains("cohesion 1.00"), "{text}");
    assert!(
        text.contains("src/core/c00.rs 0.28"),
        "key-file scores carry two decimals:\n{text}"
    );
    assert!(
        text.contains("who owns src/core?"),
        "the ownership question names a directory:\n{text}"
    );

    // A store built the same way twice renders the same bytes.
    let other = tmp("map-render-2");
    let db2 = synthetic_repo_store(&other);
    assert_eq!(render_map(&repo_map(&db2, &pinned())), text);
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

    // A budget of zero means no budget at all, so nothing is dropped. This is
    // the engine's own convention for every algorithm config.
    let full = repo_map(
        &db,
        &MapOptions {
            budget_ms: 0,
            ..pinned()
        },
    );
    assert!(!full.truncated);
    assert_eq!(full.communities.len(), 3);
    assert_eq!(full.key_files.len(), 5);

    // A budget too small to finish in may or may not fire on any given
    // machine, so what is pinned is the invariant: whatever it drops, the map
    // stays well formed and the header agrees with the flag.
    let tight = repo_map(
        &db,
        &MapOptions {
            budget_ms: 1,
            ..pinned()
        },
    );
    let header = render_map(&tight).lines().next().unwrap().to_string();
    assert_eq!(
        tight.truncated,
        header.ends_with("(truncated)"),
        "the header must say so exactly when the flag is set: {header}"
    );
    assert!(tight.key_files.len() <= 5);
    assert!(tight
        .key_files
        .iter()
        .all(|(k, s)| !k.is_empty() && *s >= 0.0));
    assert!(tight.communities.len() <= full.communities.len());

    // And the flag always reaches the header, whichever phase set it.
    let mut forced = full.clone();
    forced.truncated = true;
    assert!(render_map(&forced)
        .lines()
        .next()
        .unwrap()
        .ends_with("(truncated)"));
}

#[test]
fn map_dates_the_sync_from_the_marker_not_from_the_commits() {
    let dir = tmp("map-synced-at");
    let mut db = synthetic_repo_store(&dir);

    let sync = repo_map(&db, &pinned()).last_sync.expect("a marker");
    assert_eq!(sync.sha, sha(COMMITS - 1));
    assert_eq!(
        sync.synced_at,
        Some(SYNCED_AT),
        "the raw stamp is carried through for callers reading the map as data"
    );
    assert_eq!(sync.age_secs, Some(12 * 60));

    // The age tracks "now", not the newest commit — which is a minute older
    // than the sync and would have given a different, useless answer.
    let later = repo_map(
        &db,
        &MapOptions {
            now_ts: Some(SYNCED_AT + 3 * 3_600),
            ..MapOptions::default()
        },
    );
    assert_eq!(later.last_sync.as_ref().unwrap().age_secs, Some(3 * 3_600));
    assert!(render_map(&later)
        .lines()
        .next()
        .unwrap()
        .contains("synced 3h ago"));

    // A store written before the marker carried a stamp still names its sha,
    // just without an age.
    db.remove_prop("__mushroomdb_git_sync__", "synced_at")
        .expect("drop the stamp");
    let old = repo_map(&db, &pinned());
    let sync = old.last_sync.clone().expect("a marker");
    assert_eq!(sync.synced_at, None);
    assert_eq!(sync.age_secs, None);
    let header = render_map(&old).lines().next().unwrap().to_string();
    assert!(
        header.ends_with(&format!("· synced at {}", &sha(COMMITS - 1)[..7])),
        "no stamp, no age: {header}"
    );
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

// ---------------------------------------------------------------------------
// `brief`.
// ---------------------------------------------------------------------------

#[test]
fn brief_is_deterministic_and_within_budget() {
    let dir = tmp("brief-budget");
    let db = synthetic_repo_store(&dir);
    let a = brief(&db, &BriefOptions::default());
    let b = brief(&db, &BriefOptions::default());
    assert_eq!(a.key_files, b.key_files);
    assert_eq!(a.key_symbols, b.key_symbols);
    assert!(a.key_files.len() <= 25 && a.key_symbols.len() <= 25);

    let text = render_brief(&a, "explore <target>");
    assert!(
        text.len() <= core_api::repograph::MAX_BRIEF_BYTES,
        "{}",
        text.len()
    );
    // The brief is repository-controlled text put in a session's context
    // before its first turn: it opens with the same marker every other digest
    // opens with, and the marker is inside the budget asserted above.
    assert!(text.starts_with(UNTRUSTED_FRAMING), "{text}");
    assert!(
        text.lines()
            .nth(1)
            .unwrap()
            .starts_with("mushroomdb brief —"),
        "{text}"
    );
    assert_eq!(text.matches(UNTRUSTED_FRAMING).count(), 1, "{text}");
    assert!(
        !text.contains("ago"),
        "no relative times: the brief must be byte-stable across prompts"
    );
    assert!(text.contains("reach the graph: explore <target>"), "{text}");
    assert_eq!(
        text,
        render_brief(&b, "explore <target>"),
        "the same store renders the same bytes"
    );
}

#[test]
fn brief_on_empty_store_renders_one_helpful_line() {
    let dir = tmp("brief-empty");
    let db = open(&dir);
    let b = brief(&db, &BriefOptions::default());

    assert_eq!(b.files, 0);
    assert!(b.key_files.is_empty() && b.key_symbols.is_empty() && b.last_sync.is_none());
    let text = render_brief(&b, "explore <target>");
    assert_eq!(
        text, "mushroomdb brief — empty store; run: mushroomdb ingest-git <db> <repo>\n",
        "a session that opens on an empty store is told what is missing, not \
         how to reach a graph with nothing in it"
    );
    assert_eq!(text.lines().count(), 1);
    assert!(
        !text.contains(UNTRUSTED_FRAMING),
        "no byte of this line came out of a store, so there is nothing to mark"
    );
}

#[test]
fn brief_ranks_files_by_centrality_and_symbols_by_callers() {
    let dir = tmp("brief-ranking");
    let db = synthetic_repo_store(&dir);
    let b = brief(&db, &BriefOptions::default());

    assert_eq!(b.repo, "repo", "the marker's repo path, by its basename");
    assert_eq!(b.files, 30);
    assert_eq!(b.symbols, 12);
    assert!(b.edges > 0);
    assert_eq!(b.last_sync.as_deref(), Some(&sha(COMMITS - 1)[..7]));

    // The same ranking `map` prints, just deeper: the three hubs first.
    let ranked: Vec<&str> = b.key_files.iter().map(|(k, _)| k.as_str()).collect();
    let m = repo_map(&db, &MapOptions::default());
    let map_ranked: Vec<&str> = m.key_files.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        ranked.starts_with(&map_ranked),
        "brief and map must not rank the same files differently:\n{ranked:?}\n{map_ranked:?}"
    );

    // Symbols come by how many other symbols call them, ties on the key.
    let called: Vec<&str> = b.key_symbols.iter().map(|(k, _)| k.as_str()).collect();
    let callers = |key: &str| {
        db.weighted_edges("CALLS", None)
            .into_iter()
            .filter(|(_, dst, _)| dst == key)
            .count()
    };
    let counts: Vec<usize> = called.iter().map(|k| callers(k)).collect();
    assert!(
        counts.windows(2).all(|w| w[0] >= w[1]),
        "most called first: {called:?} {counts:?}"
    );
    assert!(
        b.key_symbols.iter().any(|(_, sig)| sig.starts_with("fn ")),
        "each symbol carries the first line of its signature: {:?}",
        b.key_symbols
    );
}

#[test]
fn a_long_brief_is_capped_by_whole_lines_and_keeps_the_reach_line() {
    let dir = tmp("brief-cap");
    let db = synthetic_repo_store(&dir);
    let b = brief(
        &db,
        &BriefOptions {
            max_files: 30,
            max_symbols: 12,
        },
    );
    // A reach line long enough that the budget cannot hold the whole listing.
    let reach = format!("explore <target> {}", "x".repeat(3_000));
    let text = render_brief(&b, &reach);
    let whole = render_brief(&b, "explore <target>");
    assert!(
        text.len() <= core_api::repograph::MAX_BRIEF_BYTES,
        "{}",
        text.len()
    );
    assert!(
        text.ends_with(&format!("reach the graph: {reach}\n")),
        "the reach line survives the cap: {text}"
    );
    assert!(
        text.lines().count() < whole.lines().count(),
        "the long reach line must have pushed listing lines out: {text}"
    );
    let kept: Vec<&str> = whole.lines().collect();
    for line in text
        .lines()
        .filter(|l| !l.starts_with("reach the graph:") && !l.starts_with("  … and "))
    {
        assert!(
            kept.contains(&line),
            "the cap drops whole lines, never half of one: {line:?}"
        );
    }

    // What was dropped is said, and counted.
    let listed = b.key_files.len() + b.key_symbols.len();
    let shown = text.lines().filter(|l| l.starts_with("  ")).count() - 1; // less the marker
    let marker = text
        .lines()
        .find(|l| l.starts_with("  … and "))
        .unwrap_or_else(|| panic!("no truncation marker in:\n{text}"));
    assert_eq!(
        marker,
        format!("  … and {} more", listed - shown),
        "the marker must count the entries actually dropped: {text}"
    );
    assert!(
        text.lines()
            .next_back()
            .unwrap()
            .starts_with("reach the graph:"),
        "the marker sits above the reach line, not below it: {text}"
    );
}

/// Symbols go before files: a path is the coarser handle, and the one a reader
/// can act on without asking the graph anything.
#[test]
fn a_tiny_budget_still_says_how_many_entries_it_dropped() {
    let dir = tmp("brief-marker");
    let db = synthetic_repo_store(&dir);
    let b = brief(&db, &BriefOptions::default());

    // Big enough for the header, a handful of lines and the reach line; far
    // too small for 25 files and 12 symbols.
    let reach = format!("explore <target> {}", "x".repeat(3_700));
    let text = render_brief(&b, &reach);

    assert!(
        text.len() <= core_api::repograph::MAX_BRIEF_BYTES,
        "{} bytes",
        text.len()
    );
    assert!(text.starts_with(UNTRUSTED_FRAMING), "{text}");
    assert!(
        text.lines()
            .nth(1)
            .unwrap()
            .starts_with("mushroomdb brief —"),
        "{text}"
    );
    assert!(
        text.contains("  … and "),
        "a listing this heavily cut must say so: {text}"
    );
    assert!(
        text.ends_with(&format!("reach the graph: {reach}\n")),
        "the reach line survives whatever the budget costs the listings"
    );
    assert!(
        !text.contains("key symbols"),
        "symbols come off before files: {text}"
    );
}

/// A key file is one the graph knows the *structure* of — something imports it
/// or calls into it. Co-change alone does not qualify: an asset directory
/// committed in one go co-changes with itself every way there is, which reads
/// to PageRank as a small tightly-knit cluster and, twenty-five entries deep,
/// fills the list with fonts and stylesheets. Those files stay reachable
/// through `context`, `impact` and `why`; they are just not what a session
/// opens on.
#[test]
fn brief_lists_only_files_something_imports_or_calls() {
    let dir = tmp("brief-edgeless");
    let mut db = synthetic_repo_store(&dir);
    // Sorts before every fixture file (`src/…`, `tests/…`), so on a tie it
    // would rank first and push a real file out of a 25-entry list.
    let asset = "aaa-asset.woff2";
    db.insert_node(
        "File",
        asset,
        vec![
            ("id".into(), core_api::Value::Str(asset.to_string())),
            ("path".into(), core_api::Value::Str(asset.to_string())),
            ("ext".into(), core_api::Value::Str("woff2".to_string())),
        ],
    )
    .expect("an asset file");
    // It is not edgeless: it was committed alongside the busiest file in the
    // repository, so the graph records the co-change — and it still does not
    // belong in a list about code structure.
    db.insert_edge("CO_CHANGED", asset, &file_key(0, 0))
        .expect("a co-change edge");
    assert!(
        db.weighted_edges("CO_CHANGED", None)
            .iter()
            .any(|(src, _, _)| src == asset),
        "the fixture must actually carry the co-change edge"
    );
    assert!(
        !db.weighted_edges("IMPORTS", None)
            .iter()
            .any(|(src, dst, _)| src == asset || dst == asset),
        "and nothing may import it"
    );

    let b = brief(&db, &BriefOptions::default());
    let listed: Vec<&str> = b.key_files.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(b.files, 31, "the count is of every file, listed or not");
    assert!(
        !listed.contains(&asset),
        "co-change alone does not make a key file: {listed:?}"
    );
    assert!(
        listed.contains(&file_key(0, 0).as_str()),
        "the imported files are still there, ranked as before: {listed:?}"
    );
    assert_eq!(listed.len(), 25, "the list is still full: {listed:?}");

    // Even asked for more entries than there are qualifying files, it pads with
    // nothing.
    let all = brief(
        &db,
        &BriefOptions {
            max_files: 100,
            max_symbols: 25,
        },
    );
    assert_eq!(
        all.key_files.len(),
        30,
        "thirty files something imports, and no more"
    );
    assert!(all.key_files.iter().all(|(k, _)| k != asset));
}

/// A store with a graph in it but no `GitSync` marker — anything ingested by
/// hand — has no repository name and no sha, and is described as the memory
/// store its own MCP surface says it is.
///
/// The marker is the one test `Surface` splits on, so a store whose session is
/// offered `explain_association` and `query` rather than `explore` must be
/// briefed with a schema rather than with two rankings it cannot act on. It
/// still has 30 `File` nodes, and the schema says so — as a label, which is
/// what it is to a session that cannot call `explore`.
#[test]
fn brief_without_a_sync_marker_is_briefed_as_a_memory_store() {
    let dir = tmp("brief-no-marker");
    let mut db = synthetic_repo_store(&dir);
    db.delete_node("__mushroomdb_git_sync__").expect("drop it");

    let b = brief(&db, &BriefOptions::default());
    assert_eq!(b.repo, "");
    assert_eq!(b.last_sync, None);
    assert_eq!(b.files, 30, "the graph itself is untouched");
    assert!(
        b.key_files.is_empty() && b.key_symbols.is_empty(),
        "the code-graph rankings belong to the code door"
    );
    let s = b
        .schema
        .as_ref()
        .expect("no marker means the memory surface");
    let labels: Vec<&str> = s.labels.iter().map(|l| l.label.as_str()).collect();
    assert!(
        labels.contains(&"File") && labels.contains(&"Symbol"),
        "{labels:?}"
    );

    let text = render_brief(&b, "explore <target>");
    assert!(text.starts_with(UNTRUSTED_FRAMING), "{text}");
    let header = text.lines().nth(1).unwrap();
    assert!(
        header.starts_with("mushroomdb brief — ") && header.contains(" nodes · "),
        "no name, no sha, and no empty separators where they would have been: {header}"
    );
    assert!(text.contains("reach the graph: explore <target>"), "{text}");
}

/// A store with no files but plenty in it — a memory graph — is not an empty
/// store, and gets a header and a way in rather than "run ingest-git".
#[test]
fn brief_on_a_store_with_no_files_still_says_how_to_reach_it() {
    let dir = tmp("brief-memory-only");
    let mut db = open(&dir);
    db.insert_node(
        "Person",
        "person:1",
        vec![("name".into(), core_api::Value::Str("Ada".to_string()))],
    )
    .expect("a node that is not a file");
    db.insert_node(
        "Person",
        "person:2",
        vec![("name".into(), core_api::Value::Str("Grace".to_string()))],
    )
    .expect("another");
    db.insert_edge("KNOWS", "person:1", "person:2")
        .expect("edge");

    let b = brief(&db, &BriefOptions::default());
    assert_eq!((b.files, b.symbols), (0, 0));
    assert_eq!(b.edges, 1);

    let text = render_brief(&b, "explore <target>");
    assert!(
        text.lines()
            .nth(1)
            .expect("a header")
            .starts_with("mushroomdb brief — 2 nodes · 1 edge · 1 label"),
        "a store with a graph in it is not an empty store: {text}"
    );
    assert!(
        text.ends_with("reach the graph: explore <target>\n"),
        "{text}"
    );
}

// ---------------------------------------------------------------------------
// `brief` on a memory store: the schema, and one worked call per question
// kind.
//
// The first association run showed the graph arm spending turn after turn
// probing Cypher for the store's labels and edge types before it could ask
// anything, and never reaching for the purpose-built tools at all. Everything
// below is that failure, written down: what the schema section has to name,
// and that each question kind arrives already worked into a call.
// ---------------------------------------------------------------------------

/// The seeded memory store every test below reads: two labels, one rule that
/// derives an edge type from a key match, a hand-written edge type, and two
/// roles.
///
/// `person:ada` is deliberately the alphabetically first `Person`, so the
/// worked calls that name "a key with edges" name a key the test can predict.
fn seeded_memory_store(name: &str) -> core_api::GraphDb<core_storage::fs::RealFs> {
    use core_api::schema::Schema;
    use core_api::{Predicate, RoleDef, RuleDef, Value};

    let dir = tmp(name);
    let mut db = open(&dir);
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![RuleDef {
            name: "assigned_to".into(),
            src_label: "Person".into(),
            dst_label: "Project".into(),
            predicate: Predicate::KeyMatch {
                field: "project_id".into(),
            },
            edge_type: "ASSIGNED_TO".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }],
        views: vec![],
        roles: vec![
            RoleDef {
                name: "auditor".into(),
                keys: vec![],
                labels: vec!["Person".into(), "Project".into()],
                write: None,
            },
            RoleDef {
                name: "analyst".into(),
                keys: vec![],
                labels: vec!["Person".into()],
                write: None,
            },
        ],
    })
    .expect("schema");

    for (key, name, stage) in [
        ("project:apollo", "Apollo", "live"),
        ("project:borealis", "Borealis", "draft"),
    ] {
        db.insert_node(
            "Project",
            key,
            vec![
                ("name".into(), Value::Str(name.to_string())),
                ("stage".into(), Value::Str(stage.to_string())),
            ],
        )
        .expect("project");
    }
    for (key, name, team, project) in [
        ("person:ada", "Ada", "core", "project:apollo"),
        ("person:bob", "Bob", "core", "project:apollo"),
        ("person:cy", "Cy", "web", "project:borealis"),
    ] {
        db.insert_node(
            "Person",
            key,
            vec![
                ("name".into(), Value::Str(name.to_string())),
                ("team".into(), Value::Str(team.to_string())),
                ("project_id".into(), Value::Str(project.to_string())),
            ],
        )
        .expect("person");
    }
    db.insert_edge("KNOWS", "person:ada", "person:bob")
        .expect("edge");
    db.insert_edge("KNOWS", "person:bob", "person:cy")
        .expect("edge");
    db
}

/// The schema section: every label with its property names and node count,
/// every edge type with the rule that derives it, the labels it runs between
/// and how many there are, how deep the history runs, and who may read it.
#[test]
fn brief_on_a_memory_store_prints_the_schema() {
    let db = seeded_memory_store("brief-memory-schema");
    let b = brief(&db, &BriefOptions::default());
    let s = b.schema.as_ref().expect("a memory store has a schema");

    assert_eq!(s.nodes, 5);
    assert_eq!(
        s.labels
            .iter()
            .map(|l| (l.label.as_str(), l.nodes))
            .collect::<Vec<_>>(),
        vec![("Person", 3), ("Project", 2)],
        "most populous first"
    );
    assert_eq!(
        s.labels[0].props,
        vec!["name".to_string(), "project_id".into(), "team".into()],
        "the union of the label's property names, sorted"
    );
    assert_eq!(
        s.edge_types
            .iter()
            .map(|t| (t.edge_type.as_str(), t.rule.as_deref(), t.edges))
            .collect::<Vec<_>>(),
        vec![("ASSIGNED_TO", Some("assigned_to"), 3), ("KNOWS", None, 2),],
        "most numerous first, and a derived type names its rule"
    );
    assert_eq!(s.edge_types[0].src, vec!["Person".to_string()]);
    assert_eq!(s.edge_types[0].dst, vec!["Project".to_string()]);
    assert_eq!(
        s.roles,
        vec![
            ("analyst".to_string(), vec!["Person".to_string()]),
            (
                "auditor".to_string(),
                vec!["Person".to_string(), "Project".to_string()]
            ),
        ],
        "roles.json, sorted by name"
    );

    let text = render_brief(&b, "query '<cypher>'");
    assert!(text.starts_with(UNTRUSTED_FRAMING), "{text}");
    for line in [
        "mushroomdb brief — 5 nodes · 5 edges · 2 labels",
        "labels:",
        "  Person (3) — name, project_id, team",
        "  Project (2) — name, stage",
        "edge types:",
        "  ASSIGNED_TO (3) — rule assigned_to — Person → Project",
        "  KNOWS (2) — Person → Person",
        &format!("history: {} commits", s.commits),
        "roles: analyst (Person) · auditor (Person, Project)",
    ] {
        assert!(
            text.lines().any(|l| l == line),
            "the schema must print {line:?}:\n{text}"
        );
    }
    assert!(s.commits > 0, "a store that was written to has a history");
    assert!(
        text.ends_with("reach the graph: query '<cypher>'\n"),
        "{text}"
    );
}

/// One worked call per question kind, in order, with the store's own keys,
/// labels and edge types already substituted in — so the call can be made
/// rather than researched.
#[test]
fn brief_on_a_memory_store_works_one_call_per_question_kind() {
    let db = seeded_memory_store("brief-memory-recipes");
    let b = brief(&db, &BriefOptions::default());
    let s = b.schema.as_ref().expect("a memory store has a schema");

    let got: Vec<(&str, &str)> = s
        .recipes
        .iter()
        .map(|r| (r.question.as_str(), r.call.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("why", "explain_association person:ada project:apollo"),
            ("relationships", "node_edges person:ada"),
            // The newest commit `edges_at` accepts, which is one below the
            // count on a store nothing has pruned — the off-by-one that made
            // this recipe `CommitOutOfRange` is pinned by
            // `the_as_of_recipe_names_a_commit_edges_at_accepts`.
            ("as of", &*format!("edges_at person:ada {}", s.commits - 1),),
            // `project_id`, not `name`: the field the `assigned_to` rule
            // reads, so the call shown is one that would really lose and gain
            // an edge. Only the new value stays a placeholder.
            ("what if", "what_if person:ada project_id <value>"),
            (
                "who may see",
                "query 'MATCH (n:Person) RETURN n.key LIMIT 20' role: analyst",
            ),
            (
                "how many",
                "MATCH (a:Person)-[:ASSIGNED_TO]->(b:Project) WITH b, count(a) AS n \
                 WHERE n >= 3 RETURN b.key, n",
            ),
        ],
        "every placeholder the store can fill is filled"
    );

    let text = render_brief(&b, "query '<cypher>'");
    assert!(text.contains("ask in one call:\n"), "{text}");
    for (question, call) in &got {
        assert!(
            text.lines().any(|l| l == format!("  {question}: {call}")),
            "the brief must print the worked call for {question:?}:\n{text}"
        );
    }
    // The two tools this section exists to point at are named where a reader
    // meets them, not left to a tool search.
    assert!(
        text.contains("edges_at ") && text.contains("what_if "),
        "{text}"
    );
}

/// Binding: the `as of` recipe is a call that *works*, not a line that reads
/// like one — the commit it names is inside `edges_at`'s accepted range.
///
/// `history: N commits` counts commits; `edges_at`'s `at` is a zero-based WAL
/// index whose valid range is `wal_horizon_floor..total_commits`. The count
/// and the last index are off by one, so substituting the count produced a
/// worked example that answered `CommitOutOfRange` on every store there has
/// ever been — the one failure mode a recipe must not have, since a session
/// that copies it learns the tool is broken and goes back to probing Cypher.
///
/// So the test does not read the recipe: it *runs* it. The commit argument is
/// parsed back out of the rendered brief — the bytes a session actually sees —
/// and handed to the engine.
#[test]
fn the_as_of_recipe_names_a_commit_edges_at_accepts() {
    let db = seeded_memory_store("brief-memory-as-of");
    let text = render_brief(&brief(&db, &BriefOptions::default()), "query '<cypher>'");

    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("  as of: edges_at "))
        .expect("the brief prints an `as of` recipe");
    let (key, at) = line.split_once(' ').expect("edges_at <key> <commit>");
    let at: u64 = at.parse().expect("the commit argument is a number");

    let edges = db
        .edges_at(key, at)
        .unwrap_or_else(|e| panic!("the brief's own worked call must answer: {e}"));
    assert!(
        !edges.is_empty(),
        "the recipe names a key with edges at that commit, or it teaches nothing"
    );

    // And it is the *latest* commit: one past it is out of range, which is
    // what pins the off-by-one rather than merely stepping back far enough to
    // stop failing.
    assert!(
        db.edges_at(key, at + 1).is_err(),
        "the recipe must name the newest commit edges_at accepts, got {at}"
    );
}

/// Binding: a store whose history is gone prints no `as of` recipe at all.
///
/// A WAL-truncating snapshot leaves `wal_horizon_floor == total_commits` — an
/// empty range, in which *every* commit index is out of range. Measured on the
/// association store: `mushroomdb snapshot --truncate` takes the brief from 95
/// s to 0.66 s, so this is the shape a large store will actually be in, not a
/// corner. The five remaining calls still work; a sixth that could not would
/// teach the session the tool is broken.
#[test]
fn a_store_with_no_reachable_history_shows_no_as_of_recipe() {
    let mut db = seeded_memory_store("brief-memory-truncated");
    db.snapshot().expect("fold the WAL into a snapshot");

    let b = brief(&db, &BriefOptions::default());
    let s = b.schema.as_ref().expect("still a memory store");
    let questions: Vec<&str> = s.recipes.iter().map(|r| r.question.as_str()).collect();

    // The schema itself is untouched — truncation costs history, not shape.
    assert!(!s.labels.is_empty() && !s.edge_types.is_empty());

    let text = render_brief(&b, "query '<cypher>'");
    if s.commits == 0 {
        assert!(
            !questions.contains(&"as of"),
            "no history means no `as of` call to show: {questions:?}"
        );
        assert!(!text.contains("edges_at "), "{text}");
        assert!(text.contains("history: 0 commits"), "{text}");
    } else {
        // Archives survived, so history did: then the recipe must still be
        // callable, which is the same invariant the test above pins.
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix("  as of: edges_at "))
            .expect("history means an `as of` recipe");
        let (key, at) = line.split_once(' ').expect("edges_at <key> <commit>");
        let at: u64 = at.parse().expect("a commit number");
        db.edges_at(key, at)
            .unwrap_or_else(|e| panic!("the brief's own worked call must answer: {e}"));
    }
    // Whatever happened to the history, the other five calls are still there.
    for question in ["why", "relationships", "what if", "who may see", "how many"] {
        assert!(
            questions.contains(&question),
            "{question:?} does not depend on history: {questions:?}"
        );
    }
}

/// Timing probe for the memory-store brief on a real store. Ignored by
/// default; point `MUSHROOMDB_BENCH_STORE` at a store directory and run with
/// `--ignored`, the same contract `tests/edges_at.rs` uses.
///
/// The number that matters is the *delta* over opening the store, which every
/// hook pays whatever it then asks. `brief` was walking every edge through
/// `all_edges_for_export` — three `String`s and a provenance entry apiece —
/// which on the association store's 1.29 M derived edges cost seven seconds in
/// debug on top of the open. `edge_type_census` replaced that walk; this
/// reports what the replacement costs, so a regression has a number to fail
/// against rather than a feeling.
#[test]
#[ignore]
fn memory_brief_bench_on_large_store() {
    let Ok(path) = std::env::var("MUSHROOMDB_BENCH_STORE") else {
        eprintln!("MUSHROOMDB_BENCH_STORE unset — skipping");
        return;
    };

    let t0 = std::time::Instant::now();
    let db = core_api::GraphDb::open_with_options(
        std::path::Path::new(&path),
        core_api::OpenOptions {
            auto_migrate: false,
            repair_wal: false,
            read_only: true,
        },
    )
    .expect("open the bench store read-only");
    let opened = t0.elapsed();
    eprintln!("open (read-only):      {opened:?}");

    let t1 = std::time::Instant::now();
    let census = db.edge_type_census();
    let census_took = t1.elapsed();
    eprintln!("edge_type_census:      {census_took:?}");

    let t2 = std::time::Instant::now();
    let total = db.wal_total_commits().expect("wal commits");
    eprintln!("wal_total_commits:     {:?}", t2.elapsed());

    let t3 = std::time::Instant::now();
    let b = brief(&db, &BriefOptions::default());
    let built = t3.elapsed();
    eprintln!("brief (whole):         {built:?}");

    let text = render_brief(&b, "query '<cypher>'");
    let s = b
        .schema
        .as_ref()
        .expect("the bench store is a memory store");
    eprintln!(
        "{} nodes, {} labels, {} edge types, {} edges, {total} wal commits, brief {} B",
        s.nodes,
        s.labels.len(),
        census.len(),
        b.edges,
        text.len()
    );
    assert!(text.len() <= core_api::repograph::MAX_BRIEF_BYTES);
    // Two budgets, because the brief's time is spent on two different things.
    //
    // The census is what this change owns — the walk that used to be
    // `all_edges_for_export` — and on 1.29 M edges it is a tenth of a second
    // in debug. That is the number a regression would blow, so it gets the
    // tight budget.
    //
    // The rest is `wal_total_commits`, which re-reads the WAL to learn the
    // commit the `as of` recipe may name. On this store — 166 MiB of WAL and
    // no snapshot — that is ~2.3 s, and it is also why opening the store costs
    // ninety. On a store anyone has ever snapshotted the WAL is empty and the
    // scan is free: `mushroomdb snapshot --truncate` takes the whole
    // `mushroomdb brief` on this store from 95 s to 0.7 s. So the whole-brief
    // budget is the hook's five seconds, measured in debug, which is three to
    // five times slower than the binary the hook runs.
    assert!(
        census_took < std::time::Duration::from_secs(1),
        "the edge-type census took {census_took:?} on {} edges",
        b.edges
    );
    assert!(
        built < std::time::Duration::from_secs(5),
        "describing the store took {built:?} (open was {opened:?})"
    );
}

/// The same store renders the same bytes, inside the same budget — the brief
/// is cached for a whole session, so two prompts must not disagree about what
/// the store is.
#[test]
fn a_memory_brief_is_byte_stable_and_within_budget() {
    let db = seeded_memory_store("brief-memory-stable");
    let one = render_brief(&brief(&db, &BriefOptions::default()), "query '<cypher>'");
    let two = render_brief(&brief(&db, &BriefOptions::default()), "query '<cypher>'");
    assert_eq!(one, two, "the same store renders the same bytes");
    assert!(
        one.len() <= core_api::repograph::MAX_BRIEF_BYTES,
        "{}",
        one.len()
    );
    assert!(
        !one.contains("ago"),
        "no relative times: the brief must be byte-stable across prompts"
    );
    assert_eq!(one.matches(UNTRUSTED_FRAMING).count(), 1, "{one}");
}

/// A store with more schema than the budget holds loses schema lines from the
/// end, counted — and never loses a worked call, which is the part that saves
/// the session a round trip.
#[test]
fn a_wide_memory_schema_is_counted_off_and_keeps_every_worked_call() {
    use core_api::Value;

    let dir = tmp("brief-memory-wide");
    let mut db = open(&dir);
    for i in 0..300 {
        db.insert_node(
            &format!("Label{i:03}"),
            &format!("node:{i:03}"),
            vec![("name".into(), Value::Str(format!("n{i}")))],
        )
        .expect("node");
    }
    db.insert_edge("KNOWS", "node:000", "node:001")
        .expect("edge");

    let b = brief(&db, &BriefOptions::default());
    let text = render_brief(&b, "query '<cypher>'");
    assert!(
        text.len() <= core_api::repograph::MAX_BRIEF_BYTES,
        "{}",
        text.len()
    );
    let marker = text
        .lines()
        .find(|l| l.starts_with("  … and "))
        .expect("a cut listing says how much it cut");
    let dropped: usize = marker
        .trim_start_matches("  … and ")
        .trim_end_matches(" more")
        .parse()
        .expect("a counted marker");
    assert!(dropped > 0 && dropped < 300, "{marker}");
    assert!(
        text.lines().filter(|l| l.starts_with("  ")).count() > 0,
        "some schema survives: {text}"
    );
    for question in [
        "why",
        "relationships",
        "as of",
        "what if",
        "who may see",
        "how many",
    ] {
        assert!(
            text.contains(&format!("  {question}: ")),
            "the {question:?} call must survive the cut:\n{text}"
        );
    }
    assert!(
        text.ends_with("reach the graph: query '<cypher>'\n"),
        "{text}"
    );
}

// ---------------------------------------------------------------------------
// `context`, `impact`, `owners`, `why`.
// ---------------------------------------------------------------------------

/// The key of the symbol named `name` in file `i` of directory `d`.
fn sym(d: usize, i: usize, name: &str) -> String {
    format!("{}#{name}", file_key(d, i))
}

/// A working tree holding one file of thirty numbered lines, so a `context`
/// call has real source to quote.
fn work_tree(name: &str) -> PathBuf {
    let dir = tmp(name);
    write_work_tree(&dir);
    dir
}

/// Fill `dir` with the working tree [`work_tree`] describes.
fn write_work_tree(dir: &Path) {
    std::fs::create_dir_all(dir.join("src/core")).expect("mkdir");
    let body: String = (1..=30).map(|n| format!("// line {n}\n")).collect();
    std::fs::write(dir.join(file_key(0, 1)), body).expect("write source");
}

/// The commits of the synthetic history that touched `path`, oldest first.
fn commits_touching(path: &str) -> Vec<usize> {
    (0..COMMITS)
        .filter(|i| touched(*i).iter().any(|f| f == path))
        .collect()
}

#[test]
fn context_on_symbol_has_source_callers_callees_and_owner() {
    let dir = tmp("context-symbol");
    let db = synthetic_repo_store(&dir);
    let repo = work_tree("context-symbol-tree");
    let key = sym(0, 1, "core::run");

    let c = context(&db, Some(repo.as_path()), &key);
    assert_eq!(c.target, Target::Symbol { key: key.clone() });
    assert!(c.candidates.is_empty(), "an exact key is never ambiguous");
    assert_eq!(c.signature.as_deref(), Some("fn core::run()"));
    assert_eq!(c.doc.as_deref(), Some("what core::run does"));
    assert_eq!(c.lines, Some((11, 21)));
    assert_eq!(c.file, file_key(0, 1));
    assert_eq!(
        c.owner.as_deref(),
        Some("Ada Example"),
        "the owner is the file's top author, by name"
    );

    // The source is the symbol's own lines, read from the working tree.
    let source = c.source.clone().expect("source from the working tree");
    assert_eq!(source.lines().count(), 11, "lines 11..=21:\n{source}");
    assert!(source.starts_with("// line 11"), "{source}");
    assert!(source.ends_with("// line 21"), "{source}");

    // Three symbols call it, each in its own file and each quoting the line it
    // does so on. Files come back with the most call sites first, then by path.
    let callers: Vec<(String, Vec<String>, Vec<u32>)> = c
        .callers
        .iter()
        .map(|s| (s.file.clone(), s.symbols.clone(), s.lines.clone()))
        .collect();
    assert_eq!(
        callers,
        vec![
            (file_key(0, 3), vec![sym(0, 3, "core::load")], vec![15]),
            (file_key(0, 4), vec![sym(0, 4, "core::save")], vec![16]),
            (file_key(1, 2), vec![sym(1, 2, "web::render")], vec![20]),
        ],
        "call sites are grouped by the file the calls sit in"
    );
    assert_eq!(c.callers_not_shown, 0);
    assert_eq!(c.callees, vec![(sym(0, 0, "core::init"), 13)]);

    // The file's own facts come along: what imports it, what it changes with.
    assert_eq!(c.imports, vec![file_key(0, 0)]);
    assert!(c.recent_commits.len() <= 5 && !c.recent_commits.is_empty());
    let text = render_context(&c);
    assert!(
        text.lines().count() <= 60,
        "{} lines:\n{text}",
        text.lines().count()
    );
    assert!(
        text.contains("// line 11"),
        "the excerpt is printed:\n{text}"
    );
    assert!(
        text.contains("Ada Example") && !text.contains("@example.test"),
        "{text}"
    );
}

/// The working tree is read only when the caller asks for it. Without a body
/// the answer is a pointer — the file and the line range to open — and the
/// graph facts, which is what an assistant needs to decide whether to open the
/// file at all.
#[test]
fn context_with_reads_the_working_tree_only_when_asked() {
    let dir = tmp("context-options");
    let db = synthetic_repo_store(&dir);
    let repo = work_tree("context-options-tree");
    let key = sym(0, 1, "core::run");

    let pointer = context_with(&db, Some(repo.as_path()), &key, &ContextOptions::default());
    assert_eq!(pointer.source, None, "the default reads no working tree");
    assert_eq!(
        pointer.lines,
        Some((11, 21)),
        "the line range is a graph fact and stays"
    );
    let text = render_context(&pointer);
    assert!(
        text.contains(&format!("  at {}:11-21\n", file_key(0, 1))),
        "the pointer is one line a reader can open:\n{text}"
    );
    assert!(!text.contains("// line 11"), "no body quoted:\n{text}");

    let full = context_with(
        &db,
        Some(repo.as_path()),
        &key,
        &ContextOptions { source: true },
    );
    assert_eq!(
        full,
        context(&db, Some(repo.as_path()), &key),
        "`context` is `context_with` asking for the body"
    );
    let full_text = render_context(&full);
    assert!(
        full_text.contains("// line 11"),
        "the body is quoted:\n{full_text}"
    );
    assert!(
        full_text.len() > text.len(),
        "the pointer is the shorter answer: {} vs {}",
        text.len(),
        full_text.len()
    );
}

/// A `CALLS` edge is written once however many times the call is written, so
/// counting edges reports a symbol called twelve times from six functions as
/// six call sites. `context` reads the caller's `call_lines`, which records
/// every site, and groups them by the file they sit in.
#[test]
fn context_lists_every_call_site() {
    let dir = tmp("context-call-sites");
    let mut db = synthetic_repo_store(&dir);
    let target = sym(0, 1, "core::run");
    let caller = sym(0, 3, "core::load");

    // The same caller, the same one edge, three lines.
    let sites = core_api::Value::List(
        [15, 40, 88]
            .iter()
            .map(|n| core_api::Value::Str(format!("{target}\t{n}")))
            .collect(),
    );
    db.set_prop(&caller, "call_lines", sites).expect("sites");

    let c = context(&db, None, &target);
    let group = c
        .callers
        .iter()
        .find(|g| g.file == file_key(0, 3))
        .expect("the caller's file");
    assert_eq!(
        group.lines,
        vec![15, 40, 88],
        "every site, not just the first"
    );
    assert_eq!(group.sites, 3);
    assert_eq!(group.symbols, vec![caller.clone()]);
    assert_eq!(
        c.callers.first().map(|g| g.file.clone()),
        Some(file_key(0, 3)),
        "the file with the most call sites comes first"
    );
    assert_eq!(c.callers_not_shown, 0);

    let text = render_context(&c);
    assert!(
        text.contains(&format!("{}: 15, 40, 88", file_key(0, 3))),
        "the digest names the file and every line in it:\n{text}"
    );

    // `why` quotes the same sites for the same edge.
    let w = why(&db, &caller, &target);
    let call = w
        .links
        .iter()
        .find(|l| l.edge_type == "CALLS")
        .expect("load calls run");
    assert_eq!(
        call.evidence,
        vec![format!("{caller} calls {target} at lines 15, 40, 88")]
    );
}

#[test]
fn context_bare_name_ambiguous_lists_candidates() {
    let dir = tmp("context-bare");
    let mut db = synthetic_repo_store(&dir);

    // A bare name that only one symbol carries resolves to that symbol.
    let one = context(&db, None, "core::flush");
    assert_eq!(
        one.target,
        Target::Symbol {
            key: sym(0, 5, "core::flush")
        }
    );
    assert!(one.candidates.is_empty());
    assert!(
        one.source.is_none(),
        "the fixture's repo path does not exist, so there is no source to read"
    );

    // Give a second symbol the same name and the answer becomes the choice.
    db.set_prop(
        &sym(1, 0, "web::serve"),
        "name",
        core_api::Value::Str("core::flush".into()),
    )
    .expect("rename");
    let two = context(&db, None, "core::flush");
    assert_eq!(
        two.candidates,
        vec![sym(0, 5, "core::flush"), sym(1, 0, "web::serve")],
        "both candidates, sorted by key"
    );
    assert!(
        two.signature.is_none() && two.callers.is_empty() && two.file.is_empty(),
        "an ambiguous target fills nothing else in"
    );
    let text = render_context(&two);
    assert!(text.contains("ambiguous"), "{text}");
    assert!(text.contains(&sym(1, 0, "web::serve")), "{text}");

    // A name nothing carries is an answer too, not an error.
    let none = context(&db, None, "no::such::thing");
    assert_eq!(
        none.target,
        Target::Unknown {
            target: "no::such::thing".into()
        }
    );
    assert!(none.candidates.is_empty());
    assert!(
        render_context(&none).contains("unknown: no::such::thing"),
        "{}",
        render_context(&none)
    );
}

#[test]
fn context_never_reads_outside_the_repo() {
    let root = tmp("context-escape");
    let repo = root.join("repo");
    write_work_tree(&repo);
    // A file next to the working tree, of the kind nobody wants quoted into an
    // assistant's context.
    let secret = root.join("secret.env");
    std::fs::write(&secret, "TOKEN=hunter2\n").expect("write secret");

    let dir = tmp("context-escape-db");
    let mut db = synthetic_repo_store(&dir);
    // A `File` key is not constrained to a repo-relative path: anything that
    // can write a node can choose one. Each of these would read the secret if
    // the key were joined to the repository root unchecked.
    let mut escapes = vec![
        secret.to_string_lossy().to_string(),
        "../secret.env".to_string(),
        "src/core/../../secret.env".to_string(),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&secret, repo.join("link.env")).expect("symlink");
        escapes.push("link.env".to_string());
    }
    for key in &escapes {
        db.insert_node(
            "File",
            key,
            vec![
                ("id".into(), core_api::Value::Str(key.clone())),
                ("path".into(), core_api::Value::Str(key.clone())),
            ],
        )
        .expect("file");
    }

    for key in &escapes {
        let c = context(&db, Some(repo.as_path()), key);
        assert_eq!(c.target, Target::File { path: key.clone() });
        assert!(
            c.source.is_none(),
            "{key} must not be read from outside the repository: {:?}",
            c.source
        );
        assert!(
            !render_context(&c).contains("hunter2"),
            "and nothing of it may reach a rendered line"
        );
    }

    // A key inside the tree still reads, so this pins refusal and not breakage.
    let inside = context(&db, Some(repo.as_path()), &file_key(0, 1));
    assert!(
        inside.source.expect("source").starts_with("// line 1"),
        "a repo-relative key is unaffected"
    );
}

#[test]
fn context_on_file_lists_importers_partners_commits() {
    let dir = tmp("context-file");
    let db = synthetic_repo_store(&dir);
    let path = file_key(0, 0);

    let c = context(&db, None, &path);
    assert_eq!(c.target, Target::File { path: path.clone() });
    assert_eq!(c.file, path);
    assert_eq!(c.owner.as_deref(), Some("Ada Example"));
    assert!(c.imports.is_empty(), "the hub imports nothing itself");
    assert_eq!(
        c.importers,
        (1..9).map(|i| file_key(0, i)).collect::<Vec<_>>(),
        "importers are sorted by key and capped"
    );
    assert!(
        c.partners
            .iter()
            .any(|(k, s)| *k == file_key(0, 1) && (*s - 1.0).abs() < 1e-9),
        "it changes with the other two files every core commit touches: {:?}",
        c.partners
    );
    assert!(
        c.partners.windows(2).all(|w| w[0].1 >= w[1].1),
        "partners are ranked by score: {:?}",
        c.partners
    );

    // The five newest commits that touched it, newest first.
    let want: Vec<String> = commits_touching(&path)
        .into_iter()
        .rev()
        .take(5)
        .map(sha)
        .collect();
    let got: Vec<String> = c.recent_commits.iter().map(|(s, _, _)| s.clone()).collect();
    assert_eq!(got, want);
    assert!(c.recent_commits.windows(2).all(|w| w[0].1 >= w[1].1));

    // What has been said about it.
    assert_eq!(
        c.notes,
        vec![(
            "note:0001".to_string(),
            "the core entry point is worth reading first".to_string()
        )]
    );
    assert_eq!(
        c.concepts,
        vec![("concept:startup".to_string(), "startup path".to_string())]
    );
}

/// The same for a file. A file has no line range, so a body-less answer has no
/// pointer line either: what is left is the file's own facts, and the flag is
/// the only thing between the two answers.
#[test]
fn context_with_on_a_file_answers_from_the_graph_alone() {
    let dir = tmp("context-options-file");
    let db = synthetic_repo_store(&dir);
    // The hub file: the one with importers, partners and notes on it. The
    // shared tree does not carry it, so this test writes it in — a body has to
    // be there for `source: true` to differ from the default at all.
    let repo = work_tree("context-options-file-tree");
    let path = file_key(0, 0);
    let body: String = (1..=30).map(|n| format!("// line {n}\n")).collect();
    std::fs::write(repo.join(&path), body).expect("write the hub file");

    let pointer = context_with(&db, None, &path, &ContextOptions::default());
    assert_eq!(pointer.target, Target::File { path: path.clone() });
    assert_eq!(pointer.source, None, "the default reads no working tree");
    assert_eq!(
        pointer,
        context_with(&db, Some(repo.as_path()), &path, &ContextOptions::default()),
        "a working tree that is there changes nothing when no body was asked for"
    );

    let text = render_context(&pointer);
    assert!(
        text.contains("importers  ") && text.contains("co-change  "),
        "the file's own facts are all there:\n{text}"
    );
    assert!(
        !text.contains("  at "),
        "a file has no line range to point at:\n{text}"
    );
    assert!(
        !text.contains("where  lines"),
        "and nothing on the `where` line stands in for one:\n{text}"
    );

    // Asking for the body changes the body and nothing else.
    let full = context_with(
        &db,
        Some(repo.as_path()),
        &path,
        &ContextOptions { source: true },
    );
    assert!(
        full.source.is_some(),
        "the head of the file is quoted from the working tree"
    );
    assert_eq!(
        ContextReport {
            source: None,
            ..full.clone()
        },
        pointer,
        "the flag decides the body and nothing else"
    );
    let full_text = render_context(&full);
    assert!(
        full_text.contains("// line 1"),
        "the body is quoted:\n{full_text}"
    );
}

/// Binding: `all` is the three answers in one report — the context, the blast
/// radius of the file behind it, and who owns it — and the digest carries all
/// three inside the default budget.
#[test]
fn explore_all_composes_context_impact_and_history() {
    let dir = tmp("explore-all");
    let db = synthetic_repo_store(&dir);
    let key = sym(0, 1, "core::run");

    let r = explore(&db, None, &key, Depth::All, false);
    assert_eq!(r.target, key);
    assert_eq!(r.depth, Depth::All);
    assert_eq!(r.context.target, Target::Symbol { key: key.clone() });
    assert_eq!(
        r.context.source, None,
        "a body is the caller's to ask for, here as everywhere"
    );

    let imp = r.impact.as_ref().expect("all carries a blast radius");
    assert_eq!(
        imp.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        vec![file_key(0, 1)],
        "the blast radius is the file the symbol is defined in"
    );
    let own = r.owners.as_ref().expect("all carries ownership");
    assert_eq!(own.path, file_key(0, 1));
    assert_eq!(
        own.top.as_ref().map(|(name, _, _)| name.as_str()),
        Some("Ada Example")
    );
    assert_eq!(
        r.partners, r.context.partners,
        "the co-change partners are the context's own, not a second computation"
    );
    assert!(
        !r.partners.is_empty(),
        "the fixture's files change together"
    );

    let text = render_explore(&r, DEFAULT_EXPLORE_BYTES);
    assert!(
        text.len() <= DEFAULT_EXPLORE_BYTES,
        "{} bytes:\n{text}",
        text.len()
    );
    for want in ["callers", "impact:", "owner:"] {
        assert!(text.contains(want), "the digest is missing {want}:\n{text}");
    }
    // The partners are on the report for a caller reading it, but the digest
    // does not print them twice: `render_context`'s `co-change` line already
    // carries the same list, and a byte-budgeted digest cannot afford a copy.
    assert!(
        !text.contains("changes with"),
        "the co-change list is printed once, not twice:\n{text}"
    );
    assert!(
        text.contains("co-change  "),
        "and the one copy is the context digest's own line:\n{text}"
    );
    assert_eq!(
        text,
        render_explore(
            &explore(&db, None, &key, Depth::All, false),
            DEFAULT_EXPLORE_BYTES
        ),
        "two runs against one store agree byte for byte"
    );
}

/// Binding: each depth costs only what it was asked for. `context` reads
/// neither the blast radius nor the history, and `impact` and `history` take
/// one each.
#[test]
fn each_depth_carries_only_its_own_answer() {
    let dir = tmp("explore-depths");
    let db = synthetic_repo_store(&dir);
    let key = sym(0, 1, "core::run");

    let c = explore(&db, None, &key, Depth::Context, false);
    assert!(c.impact.is_none() && c.owners.is_none() && c.partners.is_empty());
    let text = render_explore(&c, DEFAULT_EXPLORE_BYTES);
    assert!(
        !text.contains("impact:") && !text.contains("owner:"),
        "context depth prints the context and nothing else:\n{text}"
    );

    let i = explore(&db, None, &key, Depth::Impact, false);
    assert!(i.impact.is_some(), "impact depth carries the blast radius");
    assert!(i.owners.is_none() && i.partners.is_empty());

    let h = explore(&db, None, &key, Depth::History, false);
    assert!(h.impact.is_none(), "history depth costs no blast radius");
    assert!(h.owners.is_some() && !h.partners.is_empty());
}

/// Binding: a target nothing answers to is the context's answer and nothing
/// else — there is no file to take a blast radius or an owner of.
#[test]
fn explore_on_an_unknown_target_is_the_context_answer_alone() {
    let dir = tmp("explore-unknown");
    let db = synthetic_repo_store(&dir);

    let r = explore(&db, None, "no::such::thing", Depth::All, false);
    assert_eq!(
        r.context.target,
        Target::Unknown {
            target: "no::such::thing".to_string()
        }
    );
    assert!(r.impact.is_none() && r.owners.is_none() && r.partners.is_empty());
    let text = render_explore(&r, DEFAULT_EXPLORE_BYTES);
    assert!(text.contains("unknown: no::such::thing"), "{text}");
}

/// Binding: the budget is a hard ceiling on the digest, and it is spent on
/// whole lines — a path cut in half still reads as a path, and a caller acts
/// on it.
#[test]
fn explore_renders_within_whatever_budget_it_is_given() {
    let dir = tmp("explore-budget");
    let db = synthetic_repo_store(&dir);
    let r = explore(&db, None, &sym(0, 1, "core::run"), Depth::All, false);

    let full = render_explore(&r, DEFAULT_EXPLORE_BYTES);
    for budget in [800, 400, 200, 120] {
        let text = render_explore(&r, budget);
        assert!(
            text.len() <= budget,
            "{budget}: {} bytes:\n{text}",
            text.len()
        );
        assert!(
            full.starts_with(&text),
            "a capped digest is a prefix of the whole one:\n{text}"
        );
        assert!(
            text.is_empty() || text.ends_with('\n'),
            "the cut is at a line ending:\n{text:?}"
        );
    }

    // Below the header's own length the header is cut rather than dropped: a
    // reply that says which target was looked up is an answer, and a blank one
    // is not. The budget still binds it — that is the whole point of a ceiling.
    let header = full.lines().next().expect("a header");
    assert_eq!(
        render_explore(&r, header.len() + 1),
        format!("{header}\n"),
        "a budget with room for the header and its newline keeps both"
    );
    for budget in 0..=header.len() {
        let tiny = render_explore(&r, budget);
        assert!(
            tiny.len() <= budget,
            "budget {budget} exceeded by {} bytes:\n{tiny:?}",
            tiny.len()
        );
        if budget > 0 {
            assert!(
                header.starts_with(tiny.trim_end_matches('\n')),
                "the cut header is a prefix of the whole one:\n{tiny:?}"
            );
        }
    }
}

/// Binding: a target whose own name fills the budget is cut on a character
/// boundary, never mid-rune — the digest is text an assistant reads, and half a
/// character is not text.
#[test]
fn a_target_too_long_for_the_budget_is_cut_on_a_character_boundary() {
    let dir = tmp("explore-wide-target");
    let db = synthetic_repo_store(&dir);
    // Multi-byte throughout, so almost every byte offset is mid-character.
    let target = "ünbekannt::".repeat(40);

    let r = explore(&db, None, &target, Depth::All, false);
    for budget in 8..80 {
        let text = render_explore(&r, budget);
        assert!(text.len() <= budget, "budget {budget}: {}", text.len());
        // `String` cannot hold invalid UTF-8, so the check that matters is that
        // the render did not panic slicing one — and that what came back is a
        // prefix of the line it cut.
        let head = render_explore(&r, DEFAULT_EXPLORE_BYTES)
            .lines()
            .next()
            .expect("a header")
            .to_string();
        assert!(
            head.starts_with(text.trim_end_matches('\n')),
            "budget {budget}: {text:?} is not a prefix of {head:?}"
        );
    }
}

/// Binding: the four depth names parse, and nothing else does.
#[test]
fn depth_parses_the_four_names_and_no_others() {
    for (name, want) in [
        ("context", Depth::Context),
        ("impact", Depth::Impact),
        ("history", Depth::History),
        ("all", Depth::All),
    ] {
        assert_eq!(Depth::parse(name), Some(want), "{name}");
    }
    for bad in ["", "Context", "ALL", "everything", "context "] {
        assert_eq!(Depth::parse(bad), None, "{bad:?} must not parse");
    }
}

#[test]
fn impact_marks_partners_in_the_diff_as_modified() {
    let dir = tmp("impact-modified");
    let db = synthetic_repo_store(&dir);
    let (a, b) = (file_key(0, 0), file_key(0, 1));
    let modified: BTreeSet<String> = [a.clone(), b.clone()].into_iter().collect();

    let r = impact(
        &db,
        std::slice::from_ref(&a),
        &modified,
        &ImpactOptions::default(),
    );
    assert_eq!(r.files.len(), 1);
    assert!(r.unknown.is_empty());
    let f = &r.files[0];
    assert_eq!(f.path, a);
    assert_eq!(f.owner.as_deref(), Some("Ada Example"));
    let inside = f
        .partners
        .iter()
        .find(|p| p.path == b)
        .expect("the partner in the diff");
    assert!(inside.modified, "it is in the caller's modified set");
    assert!((inside.score - 1.0).abs() < 1e-9);
    assert!(
        f.partners.iter().any(|p| !p.modified),
        "and the partners outside it are flagged the other way: {:?}",
        f.partners
    );
    assert!(f.partners.len() <= ImpactOptions::default().max_partners);

    // A partner below the threshold is not worth telling anyone about. Both
    // floors have to be off for the list to be empty: they gate two different
    // passes, one over the scored edges and one over the commits themselves.
    let strict = impact(
        &db,
        std::slice::from_ref(&a),
        &modified,
        &ImpactOptions {
            min_score: 1.01,
            min_shared_commits: 0,
            ..ImpactOptions::default()
        },
    );
    assert!(strict.files[0].partners.is_empty());

    let text = render_impact(&r);
    assert!(
        text.lines().count() <= 25,
        "{} lines:\n{text}",
        text.lines().count()
    );
    assert!(text.contains("modified"), "{text}");
}

#[test]
fn impact_lists_importers_and_symbols_used_elsewhere() {
    let dir = tmp("impact-importers");
    let db = synthetic_repo_store(&dir);
    let a = file_key(0, 0);

    let r = impact(
        &db,
        std::slice::from_ref(&a),
        &BTreeSet::new(),
        &ImpactOptions::default(),
    );
    let f = &r.files[0];
    assert_eq!(
        f.importers
            .iter()
            .map(|p| p.path.clone())
            .collect::<Vec<_>>(),
        (1..7).map(|i| file_key(0, i)).collect::<Vec<_>>(),
        "importers are sorted by key and capped at max_importers"
    );
    assert!(
        f.importers.iter().all(|p| !p.modified),
        "nothing was modified"
    );
    assert_eq!(
        f.symbols_used_elsewhere,
        vec![(sym(0, 0, "core::init"), 3)],
        "its one symbol is called from three other files"
    );

    // A file whose symbols nobody else calls says so by staying empty.
    let leaf = impact(
        &db,
        &[file_key(0, 5)],
        &BTreeSet::new(),
        &ImpactOptions::default(),
    );
    assert!(leaf.files[0].symbols_used_elsewhere.is_empty());
}

#[test]
fn impact_reports_unknown_paths() {
    let dir = tmp("impact-unknown");
    let db = synthetic_repo_store(&dir);
    let files = vec!["nope/gone.rs".to_string(), file_key(0, 0)];

    let r = impact(&db, &files, &BTreeSet::new(), &ImpactOptions::default());
    assert_eq!(r.unknown, vec!["nope/gone.rs".to_string()]);
    assert_eq!(r.files.len(), 1, "the known path is still reported");
    let text = render_impact(&r);
    assert!(text.contains("unknown: nope/gone.rs"), "{text}");
    assert!(text.lines().count() <= 25);
}

/// Binding: a tree full of untracked artefacts names three of them and counts
/// the rest, instead of spending the whole 25-line budget on paths the graph
/// was never asked about.
#[test]
fn impact_caps_the_unknown_paths_it_names() {
    let dir = tmp("impact-unknown-cap");
    let db = synthetic_repo_store(&dir);
    let mut files: Vec<String> = (0..20)
        .map(|i| format!("out/artefact-{i:02}.json"))
        .collect();
    files.push(file_key(0, 0));

    let r = impact(&db, &files, &BTreeSet::new(), &ImpactOptions::default());
    assert_eq!(r.unknown.len(), 20, "the report still holds them all");

    let text = render_impact(&r);
    let named = text.lines().filter(|l| l.starts_with("unknown: ")).count();
    assert_eq!(named, 3, "at most three are named: {text}");
    assert!(text.contains("…and 17 more unknown"), "{text}");
    assert!(
        text.contains(&file_key(0, 0)),
        "the analysis must survive the noise: {text}"
    );
    assert!(text.lines().count() <= 25);
}

#[test]
fn owners_share_and_quarters_from_commits() {
    let dir = tmp("owners-share");
    let db = synthetic_repo_store(&dir);
    let path = file_key(0, 0);

    let o = owners(&db, &path, None).expect("a file the store knows");
    assert_eq!(o.path, path);
    let (name, key, share) = o.top.clone().expect("a top author");
    assert_eq!(
        (name.as_str(), key.as_str()),
        ("Ada Example", "a@example.test")
    );
    let mine = commits_touching(&path)
        .into_iter()
        .filter(|i| commit_author(*i) == "a@example.test")
        .count();
    let all = commits_touching(&path).len();
    assert!(
        (share - mine as f64 / all as f64).abs() < 1e-9,
        "share is that author's commits over the file's: {share}"
    );

    assert_eq!(
        o.knows.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        vec!["Ada Example".to_string()],
        "only the author who owns the files it changes with knows it"
    );
    let (last_sha, last_ts, subject) = o.last_touch.clone().expect("a last touch");
    let newest = *commits_touching(&path).last().expect("commits");
    assert_eq!(last_sha, sha(newest)[..7]);
    assert_eq!(last_ts, commit_ts(newest));
    assert_eq!(subject, format!("change {newest:02}"));

    let labels: Vec<String> = o.by_quarter.iter().map(|(q, _, _)| q.clone()).collect();
    assert_eq!(
        labels,
        vec!["2020Q4", "2021Q1", "2021Q2", "2021Q3"],
        "the last four quarters, oldest first — the fifth is out of the window"
    );
    assert_eq!(
        o.by_quarter.iter().map(|(_, _, n)| *n).collect::<Vec<_>>(),
        vec![3, 2, 3, 3]
    );
    assert_eq!(o.by_quarter[0].1, "Ada Example");

    let text = render_owners(&o);
    assert!(
        text.lines().count() <= 25,
        "{} lines:\n{text}",
        text.lines().count()
    );
    assert!(
        text.contains("Ada Example (a@example.test)"),
        "the key is printed once, in parentheses:\n{text}"
    );
    assert_eq!(
        text.matches("@example.test").count(),
        1,
        "and nowhere else:\n{text}"
    );
}

#[test]
fn owners_unknown_path_is_none() {
    let dir = tmp("owners-unknown");
    let db = synthetic_repo_store(&dir);
    assert!(owners(&db, "nope/gone.rs", None).is_none());
    // A node that is not a File is not a file's owner either.
    assert!(owners(&db, "a@example.test", None).is_none());
}

#[test]
fn why_shared_commits_newest_first() {
    let dir = tmp("why-commits");
    let db = synthetic_repo_store(&dir);
    let (a, b) = (file_key(0, 0), file_key(0, 1));

    let w = why(&db, &a, &b);
    assert!(w.unknown.is_empty() && w.path.is_empty());
    let co = w
        .links
        .iter()
        .find(|l| l.edge_type == "CO_CHANGED")
        .expect("they change together");
    assert_eq!(co.rule, "co_changed");
    assert!((co.score.expect("a score") - 1.0).abs() < 1e-9);
    assert!(co.via.is_none());

    let newest = *commits_touching(&a).last().expect("commits");
    assert!(
        co.evidence[0].starts_with(&sha(newest)[..7]),
        "the newest shared commit leads: {:?}",
        co.evidence
    );
    assert!(
        co.evidence[0].contains(&format!("change {newest:02}")),
        "with its subject: {:?}",
        co.evidence
    );
    let dates: Vec<&str> = co
        .evidence
        .iter()
        .filter_map(|e| e.split(' ').nth(1))
        .collect();
    assert!(
        dates.windows(2).all(|d| d[0] >= d[1]),
        "newest first: {dates:?}"
    );

    let text = render_why(&w);
    assert!(
        text.lines().count() <= 25,
        "{} lines:\n{text}",
        text.lines().count()
    );
    assert!(text.contains("CO_CHANGED"), "{text}");
}

#[test]
fn why_import_evidence_has_the_line() {
    let dir = tmp("why-imports");
    let db = synthetic_repo_store(&dir);
    let (a, b) = (file_key(0, 1), file_key(0, 0));

    let w = why(&db, &a, &b);
    let import = w
        .links
        .iter()
        .find(|l| l.edge_type == "IMPORTS")
        .expect("a imports b");
    assert_eq!(import.rule, "imports");
    assert_eq!(import.direction, "a→b");
    assert_eq!(
        import.evidence,
        vec![format!("{a} line 4: import {b}")],
        "the evidence quotes the line the import sits on"
    );
    assert!(render_why(&w).contains("line 4"));

    // A call is evidenced from the caller's lines — every site, since one edge
    // stands for however many times the call is written.
    let calls = why(&db, &sym(0, 1, "core::run"), &sym(0, 0, "core::init"));
    let call = calls
        .links
        .iter()
        .find(|l| l.edge_type == "CALLS")
        .expect("run calls init");
    assert_eq!(
        call.evidence,
        vec![format!(
            "{} calls {} at line 13",
            sym(0, 1, "core::run"),
            sym(0, 0, "core::init")
        )]
    );
}

#[test]
fn why_mutual_imports_render_both_directions_with_evidence() {
    let dir = tmp("why-mutual");
    let mut db = synthetic_repo_store(&dir);
    let (a, b) = (file_key(0, 0), file_key(0, 1));

    // `b` already imports `a`; make `a` import `b` too, from another line.
    db.set_prop(
        &a,
        "imports",
        core_api::Value::List(vec![core_api::Value::Str(b.clone())]),
    )
    .expect("imports");
    db.set_prop(
        &a,
        "import_lines",
        core_api::Value::List(vec![core_api::Value::Str(format!("{b}\t9"))]),
    )
    .expect("import lines");

    let w = why(&db, &a, &b);
    let imports: Vec<&core_api::repograph::WhyLink> = w
        .links
        .iter()
        .filter(|l| l.edge_type == "IMPORTS")
        .collect();
    assert_eq!(imports.len(), 2, "one edge each way: {:?}", w.links);
    assert_eq!(
        imports
            .iter()
            .map(|l| l.evidence.clone())
            .collect::<Vec<_>>(),
        vec![
            vec![format!("{a} line 9: import {b}")],
            vec![format!("{b} line 4: import {a}")],
        ],
        "each direction keeps its own line"
    );

    // Both survive rendering: a mutual pair is two facts, unlike a co-change
    // edge, whose evidence is the same set of commits whichever way it points.
    let text = render_why(&w);
    assert!(text.contains(&format!("{a} line 9: import {b}")), "{text}");
    assert!(text.contains(&format!("{b} line 4: import {a}")), "{text}");
    assert_eq!(
        text.matches("IMPORTS").count(),
        2,
        "one line per direction:\n{text}"
    );
    assert_eq!(
        text.matches("CO_CHANGED").count(),
        1,
        "while the symmetric edge is folded into one:\n{text}"
    );
    assert!(text.lines().count() <= 25, "{text}");
}

#[test]
fn why_mentions_evidence_names_the_nearest_heading() {
    let dir = tmp("why-mentions");
    let mut db = synthetic_repo_store(&dir);
    let (doc, file) = (doc_key(), doc_mentions());

    let w = why(&db, &doc, &file);
    let mention = w
        .links
        .iter()
        .find(|l| l.edge_type == "MENTIONS")
        .expect("the document mentions the file");
    assert_eq!(mention.rule, "mentions");
    assert_eq!(mention.direction, "a→b");
    assert_eq!(
        mention.evidence,
        vec![format!("{doc} mentions {file} under \"{DOC_HEADING}\"")],
        "the heading above the mention, not the document's first"
    );
    assert!(render_why(&w).contains(DOC_HEADING));

    // Without a stored body there is no line to look above, and the document's
    // first heading is what it is about.
    db.remove_prop(&doc, "body").expect("drop the body");
    let w = why(&db, &doc, &file);
    let mention = w
        .links
        .iter()
        .find(|l| l.edge_type == "MENTIONS")
        .expect("the edge is unchanged");
    assert_eq!(
        mention.evidence,
        vec![format!(
            "{doc} mentions {file} under \"{}\"",
            DOC_HEADINGS[0]
        )]
    );

    // With neither, the mention is still reported — just without a place.
    db.remove_prop(&doc, "headings").expect("drop the headings");
    let w = why(&db, &doc, &file);
    assert_eq!(
        w.links
            .iter()
            .find(|l| l.edge_type == "MENTIONS")
            .expect("the edge is unchanged")
            .evidence,
        vec![format!("{doc} mentions {file}")]
    );
}

#[test]
fn why_knows_evidence_names_the_via_file() {
    let dir = tmp("why-knows");
    let db = synthetic_repo_store(&dir);
    let (author, file) = ("a@example.test".to_string(), file_key(0, 0));

    let w = why(&db, &author, &file);
    let knows = w
        .links
        .iter()
        .find(|l| l.edge_type == "KNOWS")
        .expect("the rule links the owner of its neighbours to it");
    assert_eq!(knows.rule, "knows");
    assert_eq!(knows.direction, "a→b");
    assert_eq!(
        knows.via.as_deref(),
        Some("TOP_AUTHOR"),
        "the edge type the rule hopped over"
    );

    // The evidence names the files the author owns that share commits with it,
    // most shared first, ties on the key.
    let shared = commits_touching(&file).len();
    assert_eq!(
        knows.evidence[0],
        format!("via {} ({shared} shared commits)", file_key(0, 1))
    );
    assert!(
        knows.evidence.iter().all(|e| !e.contains(&file)),
        "the file itself is not the file it is known through: {:?}",
        knows.evidence
    );
    let text = render_why(&w);
    assert!(
        text.contains("via TOP_AUTHOR") && text.contains(&file_key(0, 1)),
        "{text}"
    );
    assert!(text.lines().count() <= 25, "{text}");
}

#[test]
fn why_falls_back_to_shortest_path() {
    let dir = tmp("why-path");
    let db = synthetic_repo_store(&dir);
    let (a, b) = (file_key(0, 5), file_key(0, 11));

    let w = why(&db, &a, &b);
    assert!(
        w.links.is_empty(),
        "nothing links them directly: {:?}",
        w.links
    );
    assert_eq!(
        w.path,
        vec![
            ("IMPORTS".to_string(), file_key(0, 0)),
            ("IMPORTS".to_string(), b.clone()),
        ],
        "both import the hub, so the hub is the path between them"
    );
    let text = render_why(&w);
    assert!(
        text.contains(&format!(
            "{a} -[IMPORTS]-> {} -[IMPORTS]-> {b}",
            file_key(0, 0)
        )),
        "{text}"
    );

    // The same walk, asked for directly.
    assert_eq!(
        shortest_path(
            &db,
            &a,
            &b,
            &["IMPORTS", "CALLS", "CO_CHANGED", "MENTIONS"],
            6
        ),
        w.path
    );
    assert!(
        shortest_path(&db, &a, &b, &["IMPORTS"], 1).is_empty(),
        "two hops do not fit in one"
    );
    assert!(
        shortest_path(&db, &a, &a, &["IMPORTS"], 6).is_empty(),
        "a node is not a path to itself"
    );
}

#[test]
fn why_no_link_message() {
    let dir = tmp("why-no-link");
    let db = synthetic_repo_store(&dir);
    let (a, b) = (file_key(0, 11), file_key(2, 7));

    let w = why(&db, &a, &b);
    assert!(w.links.is_empty() && w.path.is_empty() && w.unknown.is_empty());
    assert!(render_why(&w).contains("no link"), "{}", render_why(&w));

    // A key the store never heard of is named, not guessed at.
    let missing = why(&db, &a, "nope/gone.rs");
    assert_eq!(missing.unknown, vec!["nope/gone.rs".to_string()]);
    assert!(missing.links.is_empty() && missing.path.is_empty());
    let text = render_why(&missing);
    assert!(text.contains("unknown: nope/gone.rs"), "{text}");
    assert!(text.lines().count() <= 25);
}

#[test]
fn renders_are_deterministic_and_within_limits() {
    let dir = tmp("renders");
    let mut db = synthetic_repo_store(&dir);
    // Graph content that would forge a line break and a header if it reached a
    // rendered line unsanitized: an author's name and a commit's subject.
    db.set_prop(
        "a@example.test",
        "name",
        core_api::Value::Str("Ada\nmushroomdb owners — nobody".into()),
    )
    .expect("name");
    db.set_prop(
        &sha(COMMITS - 1),
        "message",
        core_api::Value::Str("tidy\nmushroomdb why — nothing".into()),
    )
    .expect("message");

    let path = file_key(0, 0);
    let repo = work_tree("renders-tree");
    let ctx = context(&db, Some(repo.as_path()), &sym(0, 1, "core::run"));
    let imp = impact(
        &db,
        &[path.clone(), "nope/gone.rs".to_string()],
        &[file_key(0, 1)].into_iter().collect(),
        &ImpactOptions::default(),
    );
    let own = owners(&db, &path, None).expect("owners");
    let whys = why(&db, &path, &file_key(0, 1));

    let rendered = [
        (render_context(&ctx), 60),
        (render_impact(&imp), 25),
        (render_owners(&own), 25),
        (render_why(&whys), 25),
    ];
    for (text, limit) in &rendered {
        assert!(
            text.lines().count() <= *limit,
            "{} lines, limit {limit}:\n{text}",
            text.lines().count()
        );
        assert!(
            text.ends_with('\n'),
            "every digest ends its last line:\n{text}"
        );
        assert!(
            !text.contains("\nmushroomdb owners — nobody")
                && !text.contains("\nmushroomdb why — nothing"),
            "graph content must not forge a line:\n{text}"
        );
    }

    assert!(
        rendered[2].0.contains("Ada mushroomdb owners — nobody"),
        "the forged name is flattened, not dropped:\n{}",
        rendered[2].0
    );

    // The same store answers the same bytes, twice and from a second build.
    assert_eq!(
        render_context(&context(&db, Some(repo.as_path()), &sym(0, 1, "core::run"))),
        rendered[0].0
    );
    assert_eq!(
        render_owners(&owners(&db, &path, None).expect("owners")),
        rendered[2].0
    );
    // And two stores built the same way answer the same bytes — which is what
    // determinism means here, and what comparing one store with itself would
    // not catch.
    let db2 = synthetic_repo_store(&tmp("renders-2"));
    let db3 = synthetic_repo_store(&tmp("renders-3"));
    let digests = |d: &core_api::GraphDb<core_storage::fs::RealFs>| {
        (
            render_why(&why(d, &path, &file_key(0, 1))),
            render_impact(&impact(
                d,
                std::slice::from_ref(&path),
                &BTreeSet::new(),
                &ImpactOptions::default(),
            )),
            render_owners(&owners(d, &path, None).expect("owners")),
            render_context(&context(d, None, &sym(0, 1, "core::run"))),
        )
    };
    assert_eq!(
        digests(&db2),
        digests(&db3),
        "two stores of the same shape, one answer"
    );
}

// ---------------------------------------------------------------------------
// `remember`, `recall`, and concept provenance.
// ---------------------------------------------------------------------------

#[test]
fn remember_writes_note_with_about_edges_via_rule() {
    let dir = tmp("remember-about");
    let mut db = synthetic_repo_store(&dir);
    let about = vec![file_key(0, 0), "concept:startup".to_string()];
    let input = RememberInput {
        text: "watch this boot path closely",
        about: &about,
        kind: "note",
        ts: newest_ts() + 1,
    };
    let key = remember(&mut db, &input).expect("remember");
    assert!(key.starts_with("note:"), "unexpected key: {key}");

    // The note's own `about` list derives the `ABOUT` edges via the same
    // `about_<label>` rules `ingest-git`/`structure` declare — nothing here
    // inserts an edge directly.
    let mut linked = db
        .neighbors(&key, "ABOUT", core_api::Direction::Out)
        .expect("neighbors");
    linked.sort();
    let mut want = about.clone();
    want.sort();
    assert_eq!(
        linked, want,
        "the about list must derive one ABOUT edge per key"
    );

    assert_eq!(
        db.node_ref(&key).and_then(|n| n.prop("text")),
        Some(core_api::Value::Str(
            "watch this boot path closely".to_string()
        ))
    );
    assert_eq!(
        db.node_ref(&key).and_then(|n| n.prop("source")),
        Some(core_api::Value::Str("agent".to_string())),
        "notes are attributed to the agent that wrote them"
    );
}

#[test]
fn remember_rejects_unknown_about_key() {
    let dir = tmp("remember-unknown");
    let mut db = synthetic_repo_store(&dir);
    // Two missing keys, given out of order: the error must name the first one
    // once sorted, not the first one given.
    let about = vec![file_key(0, 0), "nope:2".to_string(), "nope:1".to_string()];
    let input = RememberInput {
        text: "dangling about reference",
        about: &about,
        kind: "note",
        ts: newest_ts() + 1,
    };
    match remember(&mut db, &input) {
        Err(core_api::GraphError::KeyNotFound { key }) => {
            assert_eq!(
                key, "nope:1",
                "the first missing key, sorted, must be named"
            )
        }
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
    assert_eq!(
        db.nodes_with_label("Note").len(),
        1,
        "a rejected remember must not write a note (the fixture starts with one)"
    );
}

#[test]
fn remember_rejects_bad_text_and_bad_kind() {
    let dir = tmp("remember-validation");
    let mut db = synthetic_repo_store(&dir);
    let ts = newest_ts() + 1;

    for text in ["", "   "] {
        let input = RememberInput {
            text,
            about: &[],
            kind: "note",
            ts,
        };
        assert!(
            matches!(
                remember(&mut db, &input),
                Err(core_api::GraphError::IngestError { .. })
            ),
            "blank text must be rejected: {text:?}"
        );
    }

    let too_long = "x".repeat(4001);
    let input = RememberInput {
        text: &too_long,
        about: &[],
        kind: "note",
        ts,
    };
    assert!(matches!(
        remember(&mut db, &input),
        Err(core_api::GraphError::IngestError { .. })
    ));

    let input = RememberInput {
        text: "a fine note",
        about: &[],
        kind: "reminder",
        ts,
    };
    assert!(matches!(
        remember(&mut db, &input),
        Err(core_api::GraphError::IngestError { .. })
    ));
}

#[test]
fn remember_keys_deterministic() {
    let dir = tmp("remember-keys");
    let mut db = synthetic_repo_store(&dir);
    let ts = newest_ts() + 1;
    let input = RememberInput {
        text: "same content, twice",
        about: &[],
        kind: "note",
        ts,
    };
    let key1 = remember(&mut db, &input).expect("first remember");
    let before = db.nodes_with_label("Note").len();
    let key2 = remember(&mut db, &input).expect("second remember");
    assert_eq!(
        key1, key2,
        "the same ts and text must remember to the same key"
    );
    assert_eq!(
        db.nodes_with_label("Note").len(),
        before,
        "re-remembering the same ts and text must not duplicate the note"
    );

    let different_text = RememberInput {
        text: "different content",
        about: &[],
        kind: "note",
        ts,
    };
    let key3 = remember(&mut db, &different_text).expect("third remember");
    assert_ne!(key1, key3, "different text at the same ts must differ");

    let different_ts = RememberInput {
        text: "same content, twice",
        about: &[],
        kind: "note",
        ts: ts + 1,
    };
    let key4 = remember(&mut db, &different_ts).expect("fourth remember");
    assert_ne!(key1, key4, "the same text at a different ts must differ");
}

#[test]
fn remember_creates_the_about_rule_for_a_label_seen_for_the_first_time() {
    // A store with no rules at all — not `synthetic_repo_store`, which
    // pre-creates every `about_<label>` rule unconditionally. This is the
    // shape a store has before its first `ingest-git`/`sync`, or one whose
    // `about` names a `Concept` a semantic pass wrote since the last sync:
    // `ensure_rules_and_fulltext` has never run, so nothing has declared
    // `about_concept` yet.
    let dir = tmp("remember-self-heal");
    let mut db = open(&dir);
    db.insert_node(
        "Concept",
        "concept:fresh",
        vec![
            (
                "id".to_string(),
                core_api::Value::Str("concept:fresh".to_string()),
            ),
            (
                "name".to_string(),
                core_api::Value::Str("fresh".to_string()),
            ),
        ],
    )
    .expect("concept");
    assert!(
        db.rules().is_empty(),
        "the store must start with no rules at all: {:?}",
        db.rules()
    );

    let about = vec!["concept:fresh".to_string()];
    let input = RememberInput {
        text: "a note about a concept nothing has synced yet",
        about: &about,
        kind: "note",
        ts: 1,
    };
    let key = remember(&mut db, &input).expect("remember must self-heal the missing rule");

    let rule_names: Vec<String> = db.rules().into_iter().map(|r| r.name).collect();
    assert!(
        rule_names.contains(&"about_concept".to_string()),
        "remember must create about_concept itself: {rule_names:?}"
    );
    let linked = db
        .neighbors(&key, "ABOUT", core_api::Direction::Out)
        .expect("neighbors");
    assert_eq!(
        linked,
        vec!["concept:fresh".to_string()],
        "the edge must derive in the same commit the rule was created in, not on a later sync"
    );

    // A second remember naming the same label creates no duplicate rule.
    let rules_before = db.rules().len();
    let input2 = RememberInput {
        text: "a second note about the same concept",
        about: &about,
        kind: "note",
        ts: 2,
    };
    remember(&mut db, &input2).expect("second remember");
    assert_eq!(
        db.rules().len(),
        rules_before,
        "a label whose rule already exists must not get a second one"
    );
}

/// The digest's elision marker, as one line of `out.lines()`.
const ELISION_LINE: &str = "  …";

/// The synthetic store with the full set of fields `ingest-git`, `structure`
/// and `remember` index between them (see the doc table in
/// `docs/roadmap/v0.6-code-graph-plan.md`), enabled here because this fixture
/// is built by hand rather than by the CLI's own ingest path.
fn recall_store(dir: &Path) -> core_api::GraphDb<core_storage::fs::RealFs> {
    let mut db = synthetic_repo_store(dir);
    for (label, field) in [
        ("File", "path"),
        ("Symbol", "name"),
        ("Author", "name"),
        ("Note", "text"),
        ("Concept", "name"),
    ] {
        db.enable_fulltext(label, field).expect("fulltext");
    }
    db
}

/// Binding: a prompt with no code-shaped token in it is not a question about
/// this repository, so the digest that fires before every prompt says nothing
/// — before any search runs, not after ranking six near-random nodes.
#[test]
fn recall_is_silent_on_prose_without_identifiers() {
    let dir = tmp("recall-prose");
    let db = recall_store(&dir);
    for prompt in [
        "please explain how the server starts",
        "what changed here recently",
        "write a test for the parser",
    ] {
        assert_eq!(
            recall_digest(&db, prompt, "synthetic", MAX_OUTPUT_BYTES),
            "",
            "{prompt:?}"
        );
    }
}

/// Binding: a prompt that names symbols is answered with pointers — one
/// indented `path:line symbol — first doc line` per hit and nothing else. No
/// edge lines, and no closing nudge: the session brief already told the
/// assistant how to reach the graph.
#[test]
fn recall_prints_pointers_for_a_named_symbol() {
    let dir = tmp("recall-pointers");
    let db = recall_store(&dir);
    let out = recall_digest(
        &db,
        "why does core::init call web::serve?",
        "synthetic",
        MAX_OUTPUT_BYTES,
    );

    assert!(out.starts_with(UNTRUSTED_FRAMING), "{out}");
    assert!(
        out.lines()
            .nth(1)
            .unwrap_or_default()
            .starts_with("mushroomdb recall"),
        "{out}"
    );
    assert!(out.contains("src/core/c00.rs:"), "{out}");
    assert!(out.contains("core::init"), "{out}");
    assert!(
        out.contains("— what core::init does"),
        "the pointer carries the symbol's first doc line: {out}"
    );
    assert!(out.contains("src/web/w00.rs:"), "{out}");
    assert!(!out.contains(" -> "), "no edge lines: {out}");
    assert!(
        !out.contains("(query the mushroomdb MCP tools"),
        "no nudge: {out}"
    );
    for line in out.lines().skip(2) {
        assert!(
            line.starts_with("  "),
            "every line under the header is one pointer: {line:?} in\n{out}"
        );
    }
    assert!(out.len() <= MAX_OUTPUT_BYTES, "{} bytes", out.len());
}

/// Binding: a `File` hit has no symbol and no line to point at, so it is its
/// path and what the graph says the file is.
#[test]
fn recall_points_at_a_file_by_path_and_role() {
    let dir = tmp("recall-file");
    let mut db = recall_store(&dir);
    db.insert_node(
        "File",
        "src/web/w99.rs",
        vec![
            (
                "id".to_string(),
                core_api::Value::Str("src/web/w99.rs".into()),
            ),
            (
                "path".to_string(),
                core_api::Value::Str("src/web/w99.rs".into()),
            ),
            (
                "role".to_string(),
                core_api::Value::Str("the web entry point".into()),
            ),
        ],
    )
    .expect("file");

    let out = recall_digest(
        &db,
        "what is in src/core/c00.rs?",
        "synthetic",
        MAX_OUTPUT_BYTES,
    );
    assert!(
        out.lines().any(|l| l == "  src/core/c00.rs"),
        "a file with no role is its path alone: {out}"
    );

    let out = recall_digest(
        &db,
        "what is src/web/w99.rs?",
        "synthetic",
        MAX_OUTPUT_BYTES,
    );
    assert!(
        out.lines()
            .any(|l| l == "  src/web/w99.rs — the web entry point"),
        "{out}"
    );
}

/// Binding: backticks make a word an identifier. A note or a concept is named
/// in prose, so quoting is how a prompt says "this is a thing, not a word" —
/// and without the quotes the same prompt is silent.
#[test]
fn recall_reaches_a_prose_node_through_backticks() {
    let dir = tmp("recall-backticks");
    let db = recall_store(&dir);
    let out = recall_digest(
        &db,
        "what does `startup` cover?",
        "synthetic",
        MAX_OUTPUT_BYTES,
    );
    assert!(out.contains("concept:startup"), "{out}");
    assert!(out.contains("startup path"), "{out}");
    assert_eq!(
        recall_digest(
            &db,
            "what does startup cover?",
            "synthetic",
            MAX_OUTPUT_BYTES
        ),
        "",
        "the same words unquoted are prose"
    );
}

/// Binding: a doc line written for a reader of the file is cut to an excerpt,
/// so one verbose symbol cannot take the whole budget — or, when it is the
/// first hit, leave the digest with nothing that fits and print nothing at all.
#[test]
fn recall_cuts_a_long_doc_line_to_an_excerpt() {
    let dir = tmp("recall-long-doc");
    let mut db = recall_store(&dir);
    let key = format!("{}#core::verbose", file_key(0, 0));
    let doc = format!("Verbose. {}", "explanation ".repeat(200));
    assert!(
        doc.len() > 2_000,
        "the fixture must be over-budget on its own"
    );
    db.insert_node(
        "Symbol",
        &key,
        vec![
            ("id".to_string(), core_api::Value::Str(key.clone())),
            (
                "name".to_string(),
                core_api::Value::Str("core::verbose".into()),
            ),
            ("path".to_string(), core_api::Value::Str(file_key(0, 0))),
            ("line_start".to_string(), core_api::Value::Int(99)),
            ("doc".to_string(), core_api::Value::Str(doc)),
        ],
    )
    .expect("symbol");

    let out = recall_digest(
        &db,
        "what does core::verbose do?",
        "synthetic",
        MAX_OUTPUT_BYTES,
    );
    let pointer = out
        .lines()
        .find(|l| l.contains("core::verbose"))
        .unwrap_or_else(|| panic!("expected the hit in:\n{out}"));
    assert!(
        pointer.starts_with(&format!(
            "  {}:99 core::verbose — Verbose. ",
            file_key(0, 0)
        )),
        "{pointer}"
    );
    assert!(pointer.ends_with('…'), "the cut is marked: {pointer}");
    assert!(
        pointer.len() < 300,
        "one pointer, not a paragraph: {} bytes",
        pointer.len()
    );
    assert!(out.len() <= MAX_OUTPUT_BYTES, "{} bytes", out.len());
}

/// Binding: `identifier_terms` keeps the code-shaped tokens and drops the
/// words around them — the sentence's full stop included, which has to come
/// off `render_map.` without taking the extension off `src/core.rs.`.
#[test]
fn identifier_terms_keep_code_shaped_tokens_only() {
    assert_eq!(
        identifier_terms("the `render_map` fn and web::serve and src/core.rs but not words"),
        vec!["render_map", "web::serve", "src/core.rs"]
    );
    assert_eq!(
        identifier_terms("it lives in src/core.rs."),
        vec!["src/core.rs"],
        "a path keeps its extension when the sentence ends"
    );
    assert_eq!(
        identifier_terms("look at render_map."),
        vec!["render_map"],
        "and prose loses the full stop"
    );
    for prompt in [
        "what is render_map's job",
        "what is render_map\u{2019}s job",
        "what is `render_map's` job",
        "what is render_map's.",
    ] {
        assert_eq!(
            identifier_terms(prompt),
            vec!["render_map"],
            "the possessive is the sentence's, not the name's: {prompt:?}"
        );
    }
    assert!(identifier_terms("please explain how the server starts").is_empty());
    assert!(
        identifier_terms("the code in this file").is_empty(),
        "a word that says nothing inside a repository is not an identifier"
    );
    assert_eq!(
        identifier_terms("does core::init call core::init twice"),
        vec!["core::init"],
        "a repeat is one term"
    );
    let many: String = (0..MAX_QUERY_TERMS + 5)
        .map(|i| format!("a_{i} "))
        .collect();
    assert_eq!(
        identifier_terms(&many).len(),
        MAX_QUERY_TERMS,
        "a pasted wall of code cannot turn one prompt into hundreds of probes"
    );
}

/// Binding: the digest prints the hybrid ranking, in the hybrid ranking's
/// order, across every indexed field.
///
/// The relevance floor reads a different score — the text leg's own BM25 — to
/// decide whether to print at all. That score is not comparable between fields
/// (each has its own document count and average length), so if it ever leaked
/// into the ordering, hits from a small field would jump ahead of hits from a
/// large one. This fixture spans five fields and pins the order against
/// `search_hybrid` itself.
#[test]
fn recall_prints_hits_in_the_hybrid_ranking_order() {
    let dir = tmp("recall-hybrid-order");
    let db = recall_store(&dir);

    let prompt = "does core::init or web::serve belong in src/core/c00.rs?";
    let out = recall_digest(&db, prompt, "synthetic", 4000);

    // What `recall_digest` does internally, spelled out here against the
    // public API: every identifier searched as a phrase, best fused score per
    // key across the fields, then score descending and key ascending.
    let query: String = identifier_terms(prompt)
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<String>>()
        .join(" OR ");
    let mut fields: Vec<String> = db.fulltext_pairs().into_iter().map(|(_, f)| f).collect();
    fields.sort();
    fields.dedup();
    let mut best: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    for field in &fields {
        for (key, score) in db.search_hybrid(field, &query, "embedding", &[], None, 6) {
            let slot = best.entry(key).or_insert(0.0);
            if score > *slot {
                *slot = score;
            }
        }
    }
    let mut ranked: Vec<(String, f64)> = best.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    // A pointer names the node by path, not by key: for a `Symbol` the key is
    // `path#name`, and for everything else it is the path itself. The path is
    // the pointer's first field, minus the `:line` when there is one — so the
    // printed order is a sequence of paths, comparable outright to the
    // ranking's. (`concept:startup` is why the `:line` is stripped from the
    // end and not at the first colon.)
    let path_of = |line: &str| -> String {
        let body = line.strip_prefix("  ").unwrap_or(line);
        let first = body.split(' ').next().unwrap_or(body);
        match first.rsplit_once(':') {
            Some((path, num)) if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) => {
                path.to_string()
            }
            _ => first.to_string(),
        }
    };
    let printed: Vec<String> = out
        .lines()
        .skip(2)
        .filter(|l| *l != ELISION_LINE)
        .map(path_of)
        .collect();
    let expected: Vec<String> = ranked
        .iter()
        .take(printed.len())
        .map(|(k, _)| k.split('#').next().unwrap_or(k).to_string())
        .collect();
    assert!(!printed.is_empty(), "expected hits in:\n{out}");
    assert_eq!(
        printed, expected,
        "the digest must print the hybrid ranking in its own order:\n{out}"
    );
}

#[test]
fn stale_concepts_detects_changed_source() {
    let dir = tmp("stale-concepts-detail");
    let db = synthetic_repo_store(&dir);
    let stale = stale_concepts(&db);
    let keys: Vec<&str> = stale.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        vec!["concept:routing"],
        "only the concept recording a hash its file no longer has is stale"
    );
    let (_, reason) = &stale[0];
    assert_eq!(
        reason,
        &file_key(1, 0),
        "the reason names the source file whose hash no longer matches"
    );

    // `map`'s own count must agree — the two must not be able to diverge.
    assert_eq!(
        repo_map(&db, &MapOptions::default()).stale_concepts,
        stale.len()
    );
}
