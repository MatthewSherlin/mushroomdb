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
    context, impact, owners, recall_digest, remember, render_context, render_impact, render_map,
    render_owners, render_why, repo_map, shortest_path, stale_concepts, why, ImpactOptions,
    MapOptions, RememberInput, Target,
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

    // Three symbols call it, each quoting the line it does so on.
    let callers: Vec<(String, u32)> = c.callers.clone();
    assert_eq!(
        callers,
        vec![
            (sym(0, 3, "core::load"), 15),
            (sym(0, 4, "core::save"), 16),
            (sym(1, 2, "web::render"), 20),
        ],
        "callers are sorted by key and carry the caller's call line"
    );
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

    // A partner below the threshold is not worth telling anyone about.
    let strict = impact(
        &db,
        std::slice::from_ref(&a),
        &modified,
        &ImpactOptions {
            min_score: 1.01,
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

    // A call is evidenced the same way, from the caller's line.
    let calls = why(&db, &sym(0, 1, "core::run"), &sym(0, 0, "core::init"));
    let call = calls
        .links
        .iter()
        .find(|l| l.edge_type == "CALLS")
        .expect("run calls init");
    assert_eq!(
        call.evidence,
        vec![format!(
            "{} line 13: call {}",
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

#[test]
fn recall_finds_notes_concepts_files_symbols_people() {
    let dir = tmp("recall-all-labels");
    let mut db = synthetic_repo_store(&dir);
    // The full set `ingest-git`, `structure` and `remember` register between
    // them (see the doc table in `docs/roadmap/v0.6-code-graph-plan.md`),
    // recreated here since this fixture is built by hand rather than by the
    // CLI's own ingest path.
    for (label, field) in [
        ("File", "path"),
        ("Symbol", "name"),
        ("Author", "name"),
        ("Note", "text"),
        ("Concept", "name"),
    ] {
        db.enable_fulltext(label, field).expect("fulltext");
    }

    // One distinctive term per label, so each contributes its own top hit.
    let prompt = "c00 OR init OR ada OR entry OR startup";
    let out = recall_digest(&db, prompt, "synthetic", 4000);
    let lines: Vec<&str> = out.lines().collect();

    // Every hit is `- key [Label] name` immediately followed by up to three
    // `    edge_type -> other[ (prop w.ww)]` lines — the brief calls for
    // "each with one strongest edge", so this checks the edge line is
    // actually there, not just the header that names the label.
    for label in ["File", "Symbol", "Author", "Note", "Concept"] {
        let marker = format!("[{label}]");
        let at = lines
            .iter()
            .position(|l| l.contains(&marker))
            .unwrap_or_else(|| panic!("expected a {label} hit in:\n{out}"));
        let edge_line = lines.get(at + 1).copied().unwrap_or("");
        assert!(
            edge_line.starts_with("    ") && edge_line.contains(" -> "),
            "expected {label}'s hit ({:?}) to be followed by its strongest \
             edge, got {edge_line:?} in:\n{out}",
            lines[at]
        );
    }
    assert!(out.contains("src/core/c00.rs"), "{out}");
    assert!(out.contains("Ada Example"), "{out}");
    assert!(
        out.contains("ABOUT -> src/core/c00.rs"),
        "the note's edge names its rule and target: {out}"
    );
    assert!(
        out.contains("DESCRIBED_IN -> src/core/c00.rs"),
        "the concept's edge names its rule and target: {out}"
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
