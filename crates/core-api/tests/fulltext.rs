/// Full-text-lite API tests.
///
/// Pins the semantics of enable_fulltext, disable_fulltext, and search.
/// Each test documents the stable, observable behavior.
use mushroomdb::{GraphDb, GraphError, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open() -> GraphDb<core_storage::fs::RealFs> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("graphdb-ft-{}-{}", n, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    GraphDb::open(&d).unwrap()
}

fn tmp_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(10000);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("graphdb-ft-dir-{}-{}", n, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

// ---------------------------------------------------------------------------
// Basic AND / OR / prefix semantics
// ---------------------------------------------------------------------------

#[test]
fn and_query_requires_all_terms() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("hello world rust".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("hello python".into()))],
    )
    .unwrap();

    // Both must have "hello" AND "rust"
    let r = db.search("body", "hello rust");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "d0");

    // "hello" alone matches both
    let r2 = db.search("body", "hello");
    assert_eq!(r2.len(), 2);
}

#[test]
fn or_query_matches_either() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("rust programming".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("python scripting".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d2",
        vec![("body".into(), Value::Str("javascript web".into()))],
    )
    .unwrap();

    let r = db.search("body", "rust OR python");
    let keys: Vec<&str> = r.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"d0"));
    assert!(keys.contains(&"d1"));
    assert!(!keys.contains(&"d2"));
}

#[test]
fn explicit_and_keyword() {
    let mut db = open();
    db.enable_fulltext("A", "f").unwrap();
    db.insert_node(
        "A",
        "a0",
        vec![("f".into(), Value::Str("alpha beta gamma".into()))],
    )
    .unwrap();
    db.insert_node(
        "A",
        "a1",
        vec![("f".into(), Value::Str("alpha only".into()))],
    )
    .unwrap();

    // "alpha AND beta" same as "alpha beta"
    let r = db.search("f", "alpha AND beta");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "a0");
}

#[test]
fn prefix_match() {
    let mut db = open();
    db.enable_fulltext("Doc", "title").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("title".into(), Value::Str("rustlang".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("title".into(), Value::Str("rusty nails".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d2",
        vec![("title".into(), Value::Str("python".into()))],
    )
    .unwrap();

    let r = db.search("title", "rust*");
    let keys: Vec<&str> = r.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"d0"));
    assert!(keys.contains(&"d1"));
    assert!(!keys.contains(&"d2"));
}

#[test]
fn prefix_and_literal_combined() {
    let mut db = open();
    db.enable_fulltext("X", "f").unwrap();
    db.insert_node(
        "X",
        "n0",
        vec![("f".into(), Value::Str("rustlang awesome".into()))],
    )
    .unwrap();
    db.insert_node(
        "X",
        "n1",
        vec![("f".into(), Value::Str("rustlang boring".into()))],
    )
    .unwrap();
    db.insert_node(
        "X",
        "n2",
        vec![("f".into(), Value::Str("python awesome".into()))],
    )
    .unwrap();

    // rust* AND awesome — only n0 has both
    let r = db.search("f", "rust* awesome");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "n0");
}

// ---------------------------------------------------------------------------
// Case-insensitivity
// ---------------------------------------------------------------------------

#[test]
fn case_insensitive_indexing_and_query() {
    let mut db = open();
    db.enable_fulltext("Msg", "text").unwrap();
    db.insert_node(
        "Msg",
        "m0",
        vec![("text".into(), Value::Str("Hello World RUST".into()))],
    )
    .unwrap();

    // All case variants of the query should match
    for q in &["hello", "HELLO", "Hello", "rust", "RUST", "World"] {
        let r = db.search("text", q);
        assert_eq!(r.len(), 1, "query {q:?} should match");
    }
}

// ---------------------------------------------------------------------------
// Incremental maintenance: update reindexes old token removed, new found
// ---------------------------------------------------------------------------

#[test]
fn set_prop_reindexes_old_tokens_removed_new_found() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("rust rocks".into()))],
    )
    .unwrap();

    // Initially finds "rust"
    assert_eq!(db.search("body", "rust").len(), 1);
    assert_eq!(db.search("body", "python").len(), 0);

    // Update to python — old tokens gone, new present
    db.set_prop("d0", "body", Value::Str("python rules".into()))
        .unwrap();

    assert_eq!(db.search("body", "rust").len(), 0, "old token must be gone");
    assert_eq!(
        db.search("body", "python").len(),
        1,
        "new token must be found"
    );
}

// ---------------------------------------------------------------------------
// Delete removes node from index
// ---------------------------------------------------------------------------

#[test]
fn delete_node_removes_from_index() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("rust programming".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("rust databases".into()))],
    )
    .unwrap();

    assert_eq!(db.search("body", "rust").len(), 2);

    db.delete_node("d0").unwrap();

    let r = db.search("body", "rust");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "d1");
}

// ---------------------------------------------------------------------------
// remove_prop removes tokens for that field
// ---------------------------------------------------------------------------

#[test]
fn remove_prop_clears_tokens() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("hello rust".into()))],
    )
    .unwrap();

    assert_eq!(db.search("body", "hello").len(), 1);

    db.remove_prop("d0", "body").unwrap();

    assert_eq!(db.search("body", "hello").len(), 0);
    assert_eq!(db.search("body", "rust").len(), 0);
}

// ---------------------------------------------------------------------------
// Unindexed field behavior: returns empty (pinned)
// ---------------------------------------------------------------------------

#[test]
fn unindexed_field_returns_empty() {
    let mut db = open();
    // No enable_fulltext call
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("hello rust".into()))],
    )
    .unwrap();

    // Stable pinned behavior: empty result, no error
    let r = db.search("body", "hello");
    assert!(
        r.is_empty(),
        "unindexed field must return empty, got: {r:?}"
    );
}

#[test]
fn search_wrong_field_returns_empty() {
    let mut db = open();
    db.enable_fulltext("Doc", "bio").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("bio".into(), Value::Str("hello".into()))],
    )
    .unwrap();

    // Searching a different (non-indexed) field returns empty
    let r = db.search("title", "hello");
    assert!(r.is_empty());
}

// ---------------------------------------------------------------------------
// Re-open rebuild identity
// ---------------------------------------------------------------------------

#[test]
fn reopen_rebuild_identical_to_never_closed() {
    // Write data with fulltext index, close, reopen, search must match.
    let dir = tmp_dir();
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.enable_fulltext("Doc", "body").unwrap();
        db.insert_node(
            "Doc",
            "d0",
            vec![("body".into(), Value::Str("rust is great".into()))],
        )
        .unwrap();
        db.insert_node(
            "Doc",
            "d1",
            vec![("body".into(), Value::Str("python django".into()))],
        )
        .unwrap();
        db.set_prop("d0", "body", Value::Str("rust and databases".into()))
            .unwrap();
    }
    // Reopen
    let db2 = GraphDb::open(&dir).unwrap();
    assert!(db2.is_fulltext_enabled("Doc", "body"));

    let r = db2.search("body", "rust");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "d0");

    let r2 = db2.search("body", "python");
    assert_eq!(r2.len(), 1);
    assert_eq!(r2[0].0, "d1");

    let r3 = db2.search("body", "great");
    assert!(r3.is_empty(), "old value tokens must be gone after update");
}

// ---------------------------------------------------------------------------
// enable_fulltext / disable_fulltext
// ---------------------------------------------------------------------------

#[test]
fn enable_fulltext_already_enabled_is_error() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    let err = db.enable_fulltext("Doc", "body").unwrap_err();
    assert!(matches!(err, GraphError::RuleInvalid { .. }));
}

#[test]
fn disable_fulltext_not_enabled_is_error() {
    let mut db = open();
    let err = db.disable_fulltext("Doc", "body").unwrap_err();
    assert!(matches!(err, GraphError::RuleNotFound { .. }));
}

#[test]
fn disable_fulltext_drops_postings() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("hello rust".into()))],
    )
    .unwrap();

    assert_eq!(db.search("body", "hello").len(), 1);

    db.disable_fulltext("Doc", "body").unwrap();

    // After disable: field not indexed, returns empty
    assert!(db.search("body", "hello").is_empty());
    assert!(!db.is_fulltext_enabled("Doc", "body"));
}

#[test]
fn re_enable_after_disable_backfills() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("rust rocks".into()))],
    )
    .unwrap();
    db.disable_fulltext("Doc", "body").unwrap();

    // Re-enable: existing nodes must be backfilled
    db.enable_fulltext("Doc", "body").unwrap();
    let r = db.search("body", "rust");
    assert_eq!(r.len(), 1);
}

// ---------------------------------------------------------------------------
// Label selectivity: only indexed-label nodes appear
// ---------------------------------------------------------------------------

#[test]
fn only_enabled_label_indexed() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap(); // only Doc, not Article
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("hello rust".into()))],
    )
    .unwrap();
    db.insert_node(
        "Article",
        "a0",
        vec![("body".into(), Value::Str("hello rust".into()))],
    )
    .unwrap();

    let r = db.search("body", "hello");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "d0"); // only Doc, not Article
}

// ---------------------------------------------------------------------------
// BM25 ranking
// ---------------------------------------------------------------------------

#[test]
fn ranking_bm25_desc() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    // d0 matches both OR-groups (alpha + beta) → higher total BM25 score.
    // d1 matches only the beta group.
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("alpha beta".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("beta gamma".into()))],
    )
    .unwrap();

    let r = db.search("body", "alpha OR beta");
    assert_eq!(r.len(), 2);
    // d0 has contributions from both OR-groups → higher BM25 score.
    assert_eq!(r[0].0, "d0");
    assert_eq!(r[1].0, "d1");
    assert!(r[0].1 > r[1].1, "d0 must score higher than d1");
    assert!(r[0].1 > 0.0 && r[1].1 > 0.0);
}

/// BM25 ranks the document with the rarer term above the document with only
/// the common term.
///
/// Corpus:
///   n0: "alpha"  — "alpha" has df=1 → high IDF
///   n1: "beta"   — "beta" has df=2  → lower IDF
///   n2: "beta"   — contributes df("beta")=2
/// Query: "alpha OR beta"
/// Expected order: n0 > n1 = n2 (n1 before n2 by key ascending tiebreak).
#[test]
fn ranking_rarer_term_wins_bm25() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "n0",
        vec![("body".into(), Value::Str("alpha".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "n1",
        vec![("body".into(), Value::Str("beta".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "n2",
        vec![("body".into(), Value::Str("beta".into()))],
    )
    .unwrap();

    let r = db.search("body", "alpha OR beta");
    assert_eq!(r.len(), 3);
    assert_eq!(r[0].0, "n0", "n0 (rare alpha) must rank first");
    assert!(r[0].1 > r[1].1, "alpha (df=1) must score above beta (df=2)");
}

// ---------------------------------------------------------------------------
// Oracle equivalence: db.search == db.scratch_search
// ---------------------------------------------------------------------------

#[test]
fn oracle_equivalence_basic() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![(
            "body".into(),
            Value::Str("I love Rust and databases".into()),
        )],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("Python is also great".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d2",
        vec![(
            "body".into(),
            Value::Str("rust AND python both useful".into()),
        )],
    )
    .unwrap();

    for q in &[
        "rust",
        "python",
        "rust OR python",
        "rust*",
        "databases",
        "nope",
    ] {
        let idx_keys: Vec<String> = db.search("body", q).into_iter().map(|(k, _)| k).collect();
        let scratch_keys: Vec<String> = db
            .scratch_search("body", q)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(idx_keys, scratch_keys, "oracle mismatch for query {q:?}");
    }
}

#[test]
fn oracle_after_update_and_delete() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("rust rocks".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("python cool".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d2",
        vec![("body".into(), Value::Str("java enterprise".into()))],
    )
    .unwrap();

    db.set_prop("d0", "body", Value::Str("python now".into()))
        .unwrap();
    db.delete_node("d2").unwrap();

    for q in &["rust", "python", "java", "rust OR python", "now", "py*"] {
        let idx_keys: Vec<String> = db.search("body", q).into_iter().map(|(k, _)| k).collect();
        let scratch_keys: Vec<String> = db
            .scratch_search("body", q)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(idx_keys, scratch_keys, "oracle mismatch for query {q:?}");
    }
}

// ---------------------------------------------------------------------------
// Snapshot persistence — declarations must survive snapshot → reopen
// ---------------------------------------------------------------------------

/// C-1 regression: enable → snapshot → reopen must restore declarations and index.
#[test]
fn declarations_survive_snapshot_and_reopen() {
    let dir = tmp_dir();
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.enable_fulltext("Article", "bio").unwrap();
        db.insert_node(
            "Article",
            "a0",
            vec![("bio".into(), Value::Str("rust embedded graph".into()))],
        )
        .unwrap();
        db.insert_node(
            "Article",
            "a1",
            vec![("bio".into(), Value::Str("python scripting".into()))],
        )
        .unwrap();
        db.snapshot().unwrap();
        // Confirm search works before close.
        let r = db.search("bio", "rust");
        assert_eq!(r.len(), 1, "pre-close search must find a0");
    }
    // Reopen from snapshot.
    {
        let db2 = GraphDb::open(&dir).unwrap();
        assert!(
            db2.is_fulltext_enabled("Article", "bio"),
            "declaration must survive snapshot"
        );
        let r = db2.search("bio", "rust");
        assert_eq!(
            r.len(),
            1,
            "index must be rebuilt after reopen from snapshot"
        );
        assert_eq!(r[0].0, "a0");
    }
}

/// Snapshot with zero declarations must not write junk to WAL.
#[test]
fn snapshot_with_no_declarations_stays_clean() {
    let dir = tmp_dir();
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("X", "x0", vec![]).unwrap();
        db.snapshot().unwrap();
    }
    // Reopen: no fulltext declarations, no errors.
    let db2 = GraphDb::open(&dir).unwrap();
    assert!(!db2.is_fulltext_enabled("X", "bio"));
    assert!(db2.search("bio", "rust").is_empty());
}

/// enable → snapshot → more writes → open_at semantics remain correct.
///
/// After snapshot(), the WAL is atomically replaced with baseline records:
/// one EnableFulltext record per enabled pair.  Post-snapshot user writes
/// follow.  open_at(i) must replay correctly at any position.
///
/// WAL structure after the workload below, counted from the floor the
/// snapshot set (the commits before it are discarded and keep their numbers):
///   floor + 0: EnableFulltext("Article","bio")   ← baseline written by snapshot()
///   floor + 1: InsertNode "a1"
///   floor + 2: InsertNode "a2"
#[test]
fn open_at_works_after_snapshot_with_declarations() {
    let dir = tmp_dir();
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.enable_fulltext("Article", "bio").unwrap(); // pre-snapshot WAL commit
        db.insert_node(
            "Article",
            "a0",
            vec![("bio".into(), Value::Str("rust".into()))],
        )
        .unwrap();
        db.snapshot().unwrap();
        // snapshot() replaces WAL with baseline: [EnableFulltext("Article","bio")]
        db.insert_node(
            "Article",
            "a1",
            vec![("bio".into(), Value::Str("python".into()))],
        )
        .unwrap(); // floor + 1
        db.insert_node(
            "Article",
            "a2",
            vec![("bio".into(), Value::Str("rust lang".into()))],
        )
        .unwrap(); // floor + 2
    }
    // The snapshot discarded what came before it; the floor is where that
    // ended, and the WAL holds exactly 3 records from there (floor..=floor+2).
    let floor = GraphDb::open(&dir).unwrap().wal_horizon_floor();
    assert!(floor > 0, "the truncating snapshot moved the floor");
    // At every commit, fulltext must be enabled (baseline is always first).
    for commit in floor..=floor + 2 {
        let snap = GraphDb::open_at(&dir, commit).unwrap();
        assert!(
            snap.is_fulltext_enabled("Article", "bio"),
            "fulltext must be enabled at commit {commit}"
        );
    }
    // CommitOutOfRange one past the end (only 3 records)…
    assert!(matches!(
        GraphDb::open_at(&dir, floor + 3),
        Err(GraphError::CommitOutOfRange { .. })
    ));
    // …and for a number the snapshot discarded.
    assert!(matches!(
        GraphDb::open_at(&dir, 0),
        Err(GraphError::CommitOutOfRange { .. })
    ));
    // At pos 2 (all WAL records), the as-of state has:
    //   - fulltext enabled (pos 0 baseline)
    //   - a0 with "rust" (pre-snapshot, loaded from the snapshot base — a
    //     truncating snapshot IS the WAL-head state)
    //   - a1 with "python" (pos 1)
    //   - a2 with "rust lang" (pos 2)
    let snap = GraphDb::open_at(&dir, floor + 2).unwrap();
    assert!(
        snap.is_fulltext_enabled("Article", "bio"),
        "fulltext must be enabled at the last commit"
    );
    let mut hits: Vec<String> = snap
        .search("bio", "rust")
        .into_iter()
        .map(|r| r.0)
        .collect();
    hits.sort_unstable();
    assert_eq!(
        hits,
        vec!["a0".to_string(), "a2".to_string()],
        "a0 (snapshot base) and a2 (WAL tail) must both match 'rust'"
    );
}

// ---------------------------------------------------------------------------
// Cypher textMatches function in WHERE position
// ---------------------------------------------------------------------------

#[test]
fn cypher_text_matches_where() {
    let mut db = open();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("rust is great".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("python scripting".into()))],
    )
    .unwrap();

    let no_params: BTreeMap<String, Value> = BTreeMap::new();

    // textMatches does per-row scratch matching — no index needed
    let rs = db
        .query(
            "MATCH (d:Doc) WHERE textMatches(d.body, 'rust') RETURN d",
            &no_params,
        )
        .unwrap();
    assert_eq!(rs.len(), 1);

    let rs2 = db
        .query(
            "MATCH (d:Doc) WHERE textMatches(d.body, 'python OR rust') RETURN d",
            &no_params,
        )
        .unwrap();
    assert_eq!(rs2.len(), 2);

    let rs3 = db
        .query(
            "MATCH (d:Doc) WHERE textMatches(d.body, 'rust*') RETURN d",
            &no_params,
        )
        .unwrap();
    assert_eq!(rs3.len(), 1);
}

// ---------------------------------------------------------------------------
// Regression: disable_fulltext with shared field (multi-label index)
// ---------------------------------------------------------------------------

/// When labels A, B, C all index field f and label A is disabled:
/// - A's nodes must vanish from search results
/// - B's and C's nodes must remain searchable (postings column kept)
/// - Disabling B and C then drops the column entirely
///
/// Pins the fix for the proptest-discovered bug: `disable_fulltext(A, f)` was
/// leaving A-node postings in the shared field column when another label still
/// indexed the same field.
#[test]
fn disable_shared_field_removes_only_disabled_label_postings() {
    let mut db = open();

    // Three nodes across three labels, all with field "f".
    db.insert_node("A", "a1", vec![("f".into(), Value::Str("alpha".into()))])
        .unwrap();
    db.insert_node("B", "b1", vec![("f".into(), Value::Str("beta".into()))])
        .unwrap();
    db.insert_node("C", "c1", vec![("f".into(), Value::Str("gamma".into()))])
        .unwrap();

    // Enable fulltext on field "f" for all three labels.
    db.enable_fulltext("A", "f").unwrap();
    db.enable_fulltext("B", "f").unwrap();
    db.enable_fulltext("C", "f").unwrap();

    // All three nodes searchable before any disable.
    assert_eq!(
        db.search("f", "alpha").len(),
        1,
        "a1 present before disable"
    );
    assert_eq!(db.search("f", "beta").len(), 1, "b1 present before disable");
    assert_eq!(
        db.search("f", "gamma").len(),
        1,
        "c1 present before disable"
    );

    // Disable label A — B and C must remain.
    db.disable_fulltext("A", "f").unwrap();
    assert_eq!(
        db.search("f", "alpha").len(),
        0,
        "a1 absent after A disabled"
    );
    assert_eq!(
        db.search("f", "beta").len(),
        1,
        "b1 still present after A disabled"
    );
    assert_eq!(
        db.search("f", "gamma").len(),
        1,
        "c1 still present after A disabled"
    );

    // Disable label B — C must remain.
    db.disable_fulltext("B", "f").unwrap();
    assert_eq!(
        db.search("f", "beta").len(),
        0,
        "b1 absent after B disabled"
    );
    assert_eq!(
        db.search("f", "gamma").len(),
        1,
        "c1 still present after B disabled"
    );

    // Disable label C — column now fully dropped; all searches empty.
    db.disable_fulltext("C", "f").unwrap();
    assert_eq!(
        db.search("f", "gamma").len(),
        0,
        "c1 absent after C disabled"
    );
    assert_eq!(
        db.search("f", "alpha").len(),
        0,
        "column dropped; no results"
    );
}

// ---------------------------------------------------------------------------
// v2: Stemming
// ---------------------------------------------------------------------------

/// "running" and "run" share a stem ("run") → both match the indexed doc.
#[test]
fn stemming_running_matches_run_doc() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("run forest run".into()))],
    )
    .unwrap();

    // Query with inflected form → same stem → must find d0.
    let r = db.search("body", "running");
    assert_eq!(r.len(), 1, "inflected query must match stemmed index");
    assert_eq!(r[0].0, "d0");

    // Stemmed base form also matches.
    let r2 = db.search("body", "run");
    assert_eq!(r2.len(), 1);
}

/// "databases" stems to "databas"; index and query both stem consistently.
#[test]
fn stemming_databases_matches() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("graph databases embedded".into()))],
    )
    .unwrap();

    let r = db.search("body", "databases");
    assert_eq!(r.len(), 1);
    let r2 = db.search("body", "database");
    assert_eq!(r2.len(), 1, "singular form must also match via stemming");
}

// ---------------------------------------------------------------------------
// v2: Phrase queries
// ---------------------------------------------------------------------------

/// Phrase match requires adjacent tokens (stemmed), not scattered tokens.
#[test]
fn phrase_adjacent_only() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    // d0: "graph" and "database" are adjacent.
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("graph database embedded".into()))],
    )
    .unwrap();
    // d1: "graph" and "database" are NOT adjacent.
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("graph embedded database".into()))],
    )
    .unwrap();

    let r = db.search("body", "\"graph database\"");
    assert_eq!(r.len(), 1, "only adjacent doc must match phrase");
    assert_eq!(r[0].0, "d0");
}

/// Phrase match uses stemmed tokens: "running fast" matches "I am running fast today".
#[test]
fn phrase_matches_stemmed_forms() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("I am running fast today".into()))],
    )
    .unwrap();

    // Both tokens stem correctly and are adjacent in the document.
    let r = db.search("body", "\"running fast\"");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "d0");
}

// ---------------------------------------------------------------------------
// v2: Negation
// ---------------------------------------------------------------------------

/// `-embedded` excludes documents containing "embedded" (stemmed).
#[test]
fn negation_excludes_matching_doc() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("graph database embedded".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("graph database".into()))],
    )
    .unwrap();

    // "-embedded graph" → d0 excluded (has "embedded"); d1 matches.
    let r = db.search("body", "-embedded graph");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "d1");
}

/// Prefix `emb*` still works and matches "embedded" in the stemmed index.
#[test]
fn prefix_emb_matches_embedded() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("graph embedded database".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("graph only".into()))],
    )
    .unwrap();

    let r = db.search("body", "emb*");
    assert_eq!(r.len(), 1, "emb* must match the doc with embedded");
    assert_eq!(r[0].0, "d0");
}

// ---------------------------------------------------------------------------
// v2: textMatches WHERE with phrase and negation
// ---------------------------------------------------------------------------

#[test]
fn cypher_text_matches_phrase() {
    let mut db = open();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("graph database embedded".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("graph embedded database".into()))],
    )
    .unwrap();

    let no_params: BTreeMap<String, Value> = BTreeMap::new();

    let rs = db
        .query(
            "MATCH (d:Doc) WHERE textMatches(d.body, '\"graph database\"') RETURN d",
            &no_params,
        )
        .unwrap();
    assert_eq!(rs.len(), 1, "only adjacent doc must match phrase in WHERE");
}

#[test]
fn cypher_text_matches_negation() {
    let mut db = open();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("graph database embedded".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("graph database".into()))],
    )
    .unwrap();

    let no_params: BTreeMap<String, Value> = BTreeMap::new();

    // "-embedded graph" → d0 excluded; d1 matches.
    let rs = db
        .query(
            "MATCH (d:Doc) WHERE textMatches(d.body, '-embedded graph') RETURN d",
            &no_params,
        )
        .unwrap();
    assert_eq!(rs.len(), 1, "negation must exclude doc with embedded");
}

// ---------------------------------------------------------------------------
// v2: Index rebuild (WAL replay identity)
// ---------------------------------------------------------------------------

/// Rebuild via WAL replay produces the same search results as the pre-close state.
/// This is the "replay-identity" test: the index is rebuilt-from-WAL (not
/// snapshot-persisted), so any stemming/position change must survive reopen.
#[test]
fn reopen_rebuild_identity_v2() {
    let dir = tmp_dir();
    let expected_keys = {
        let mut db = GraphDb::open(&dir).unwrap();
        db.enable_fulltext("Doc", "body").unwrap();
        db.insert_node(
            "Doc",
            "d0",
            vec![(
                "body".into(),
                Value::Str("running databases embedded".into()),
            )],
        )
        .unwrap();
        db.insert_node(
            "Doc",
            "d1",
            vec![("body".into(), Value::Str("python scripting".into()))],
        )
        .unwrap();
        let keys: Vec<String> = db
            .search("body", "run")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        keys
    };

    // Reopen from WAL — must rebuild identical index.
    let db2 = GraphDb::open(&dir).unwrap();
    let rebuilt_keys: Vec<String> = db2
        .search("body", "run")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        expected_keys, rebuilt_keys,
        "WAL-rebuilt index must match pre-close state"
    );
    assert!(!rebuilt_keys.is_empty(), "d0 must be found after reopen");
    assert_eq!(rebuilt_keys[0], "d0");
}

// ---------------------------------------------------------------------------
// v2: Pinned edge-case behaviors
// ---------------------------------------------------------------------------

/// Pin: a pure negation query ("-term" with no positive atom) returns empty.
///
/// The candidates may be non-empty (all nodes without "embedded"), but no
/// positive scoring happens so every group_score stays 0.0, which the
/// `if group_score > 0.0` guard suppresses.
#[test]
fn all_negation_query_returns_empty() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("graph database embedded".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("graph database".into()))],
    )
    .unwrap();

    let r = db.search("body", "-embedded");
    assert!(r.is_empty(), "pure negation query must return empty");
}

/// Pin: "graph OR -embedded" returns the same key ordering as "graph".
///
/// The negation-only OR group contributes group_score = 0.0, which the scoring
/// guard suppresses.  The effective query is therefore just "graph".
#[test]
fn negation_only_or_group_same_as_plain_query() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node(
        "Doc",
        "d0",
        vec![("body".into(), Value::Str("graph database".into()))],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("rust embedded".into()))],
    )
    .unwrap();

    let keys_plain: Vec<String> = db
        .search("body", "graph")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let keys_or_neg: Vec<String> = db
        .search("body", "graph OR -embedded")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        keys_plain, keys_or_neg,
        "negation-only OR group must not change result ordering"
    );
}

/// Phrase queries do NOT match across Value::List element boundaries.
///
/// The index inserts a position gap (> 1) between list elements.  Adjacent
/// positions require delta == 1, so cross-boundary adjacency is impossible.
#[test]
fn list_phrase_does_not_match_across_element_boundary() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    // body = ["graph", "database"] — two separate list elements; phrase must NOT match.
    db.insert_node(
        "Doc",
        "d0",
        vec![(
            "body".into(),
            Value::List(vec![
                Value::Str("graph".into()),
                Value::Str("database".into()),
            ]),
        )],
    )
    .unwrap();
    // body = "graph database" — single string; phrase MUST match.
    db.insert_node(
        "Doc",
        "d1",
        vec![("body".into(), Value::Str("graph database".into()))],
    )
    .unwrap();

    let r = db.search("body", "\"graph database\"");
    assert_eq!(
        r.len(),
        1,
        "phrase must not match across list element boundary"
    );
    assert_eq!(r[0].0, "d1", "only the single-string doc must match");
}

/// Phrase within a single list element still matches.
///
/// Tokens within one list element have consecutive positions, so a phrase that
/// fits entirely inside one element is found as expected.
#[test]
fn list_phrase_matches_within_single_element() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    // body = ["graph database", "other"]; "graph database" is adjacent within elem 0.
    db.insert_node(
        "Doc",
        "d0",
        vec![(
            "body".into(),
            Value::List(vec![
                Value::Str("graph database".into()),
                Value::Str("other".into()),
            ]),
        )],
    )
    .unwrap();

    let r = db.search("body", "\"graph database\"");
    assert_eq!(r.len(), 1, "phrase within a single list element must match");
    assert_eq!(r[0].0, "d0");
}

// ---------------------------------------------------------------------------
// Item 24: scratch_search oracle matches index for list-field values
// ---------------------------------------------------------------------------

/// After fixing scratch_search to use value_tokens_stemmed_with_positions
/// (with POSITION_GAP between list elements), the oracle must agree with the
/// indexed search for list-valued fields.  This test is the list-boundary
/// oracle case called for in item 24.
#[test]
fn scratch_search_oracle_matches_index_for_list_field() {
    let mut db = open();
    db.enable_fulltext("Doc", "tags").unwrap();

    // Two docs with list-valued tags fields.
    db.insert_node(
        "Doc",
        "d0",
        vec![(
            "tags".into(),
            Value::List(vec![
                Value::Str("graph database".into()),
                Value::Str("storage".into()),
            ]),
        )],
    )
    .unwrap();
    db.insert_node(
        "Doc",
        "d1",
        vec![(
            "tags".into(),
            Value::List(vec![
                Value::Str("search engine".into()),
                Value::Str("database".into()),
            ]),
        )],
    )
    .unwrap();

    // Both docs have "database" somewhere in their list; confirm oracle agrees.
    for q in &[
        "database",
        "graph",
        "storage",
        "search",
        "\"graph database\"",
        "graph OR search",
    ] {
        let idx_keys: Vec<String> = db.search("tags", q).into_iter().map(|(k, _)| k).collect();
        let scratch_keys: Vec<String> = db
            .scratch_search("tags", q)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            idx_keys, scratch_keys,
            "oracle mismatch for query {:?}: index={:?} scratch={:?}",
            q, idx_keys, scratch_keys
        );
    }

    // Phrase "graph database" must NOT match d1 (cross-element boundary):
    // d0 has "graph database" as a single list element → should match.
    // d1 has "search engine" and "database" in separate elements → "graph database" must not match.
    let phrase_hits: Vec<String> = db
        .search("tags", "\"graph database\"")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        phrase_hits,
        vec!["d0".to_string()],
        "phrase should only match within single element, not across list boundary"
    );

    // Confirm scratch_search also correctly does not cross boundaries.
    let scratch_phrase: Vec<String> = db
        .scratch_search("tags", "\"graph database\"")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        scratch_phrase, phrase_hits,
        "scratch_search and index must agree on list-boundary phrase behavior"
    );
}

// ---------------------------------------------------------------------------
// The index in the snapshot (ledger row 36)
// ---------------------------------------------------------------------------

/// What every search a caller could tell two stores apart by returns: scores
/// and all, so a doubled posting or a stale document length shows.
fn observable(db: &GraphDb<core_storage::fs::RealFs>) -> Vec<(String, Vec<(String, f64)>)> {
    let mut out = Vec::new();
    for field in ["body", "title"] {
        for query in [
            "rust",
            "graph",
            "\"graph database\"",
            "rust -python",
            "dat*",
            "python",
            "notes",
            "great",
            "embedded",
            "tokyo",
        ] {
            out.push((format!("{field}: {query}"), db.search(field, query)));
        }
    }
    out
}

fn doc(body: &str, title: &str) -> Vec<(String, Value)> {
    vec![
        ("body".into(), Value::Str(body.into())),
        ("title".into(), Value::Str(title.into())),
    ]
}

/// Forty documents under one declared pair, with a list value, an empty
/// string and a number among them.
fn seed(db: &mut GraphDb<core_storage::fs::RealFs>) {
    db.enable_fulltext("Doc", "body").unwrap();
    for i in 0..40 {
        let body = match i % 4 {
            0 => "rust is a great language for an embedded graph database",
            1 => "python django notes",
            2 => "a graph of data and databases in tokyo",
            _ => "rust rust rust and python",
        };
        db.insert_node("Doc", &format!("d{i}"), doc(body, &format!("notes {i}")))
            .unwrap();
    }
    db.set_prop(
        "d1",
        "body",
        Value::List(vec![
            Value::Str("rust graph".into()),
            Value::Str("database".into()),
        ]),
    )
    .unwrap();
    db.set_prop("d2", "body", Value::Str(String::new()))
        .unwrap();
    db.set_prop("d3", "body", Value::Int(7)).unwrap();
}

/// Binding: after a snapshot the index is read from it, not built again, and
/// every search answers as it did before the store was closed.
#[test]
fn a_snapshotted_index_is_loaded_not_rebuilt() {
    let dir = tmp_dir();
    let before = {
        let mut db = GraphDb::open(&dir).unwrap();
        seed(&mut db);
        db.snapshot().unwrap();
        observable(&db)
    };
    let db = GraphDb::open(&dir).unwrap();
    assert!(
        db.fulltext_loaded_from_snapshot(),
        "the snapshot carries the index and nothing replayed could have spoiled it"
    );
    assert!(db.is_fulltext_enabled("Doc", "body"));
    assert_eq!(observable(&db), before);
}

/// Binding: what is written after the snapshot is applied to the loaded
/// index — an insert, a rewrite, a removal, a delete, a pair declared late and
/// a pair withdrawn — and the store answers as one that never closed.
#[test]
fn writes_after_the_snapshot_land_on_the_loaded_index() {
    let dir = tmp_dir();
    let before = {
        let mut db = GraphDb::open(&dir).unwrap();
        seed(&mut db);
        db.enable_fulltext("Doc", "title").unwrap();
        db.snapshot().unwrap();
        db.insert_node(
            "Doc",
            "late",
            doc("a late embedded graph database", "late notes"),
        )
        .unwrap();
        db.set_prop("d0", "body", Value::Str("python only now".into()))
            .unwrap();
        db.remove_prop("d4", "body").unwrap();
        db.delete_node("d5").unwrap();
        db.insert_node("Doc", "d5", doc("rust again under an old key", "back"))
            .unwrap();
        db.disable_fulltext("Doc", "title").unwrap();
        db.insert_node("Memo", "m0", doc("a memo about rust", "memo"))
            .unwrap();
        db.enable_fulltext("Memo", "body").unwrap();
        observable(&db)
    };
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.fulltext_loaded_from_snapshot());
    assert!(!db.is_fulltext_enabled("Doc", "title"));
    assert!(db.is_fulltext_enabled("Memo", "body"));
    assert_eq!(observable(&db), before);

    // And once more through a second snapshot of the reopened store.
    let mut db = db;
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.fulltext_loaded_from_snapshot());
    assert_eq!(observable(&db), before);
}

/// Binding: a snapshot that keeps its WAL carries the index too. The open
/// does not apply again the writes that snapshot holds, so the index it read
/// is kept, and what was written after the snapshot lands on it.
#[test]
fn a_keep_wal_snapshot_loads_its_index_too() {
    let dir = tmp_dir();
    let before = {
        let mut db = GraphDb::open(&dir).unwrap();
        seed(&mut db);
        db.snapshot_with(mushroomdb::SnapshotOptions {
            keep_wal: true,
            ..Default::default()
        })
        .unwrap();
        db.insert_node("Doc", "late", doc("a late embedded graph database", "late"))
            .unwrap();
        db.set_prop("d0", "body", Value::Str("python only now".into()))
            .unwrap();
        observable(&db)
    };
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.fulltext_loaded_from_snapshot());
    assert_eq!(db.fulltext_reindexed_during_open(), 0);
    assert_eq!(observable(&db), before);
}

/// Binding: a blob that does not read back is not trusted and not fatal. The
/// store opens, builds the index as it used to, and answers the same.
#[test]
fn a_damaged_index_blob_is_rebuilt_not_trusted() {
    let dir = tmp_dir();
    let before = {
        let mut db = GraphDb::open(&dir).unwrap();
        seed(&mut db);
        db.snapshot().unwrap();
        observable(&db)
    };
    let path = dir.join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    let at = bytes
        .windows(4)
        .position(|w| w == b"MFTS")
        .expect("the snapshot carries the index blob");
    bytes[at + 40] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();

    let db = GraphDb::open(&dir).unwrap();
    assert!(!db.fulltext_loaded_from_snapshot());
    assert_eq!(observable(&db), before);
}

/// Binding: a store with no full-text pair writes no index section, so its
/// snapshot is what it always was.
#[test]
fn a_store_with_no_declared_pair_writes_no_index_section() {
    let dir = tmp_dir();
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Doc", "d0", doc("rust", "t")).unwrap();
        db.snapshot().unwrap();
    }
    let bytes = std::fs::read(dir.join("snapshot.bin")).unwrap();
    assert!(!bytes.windows(4).any(|w| w == b"MFTS"));
    let db = GraphDb::open(&dir).unwrap();
    assert!(!db.fulltext_loaded_from_snapshot());
}

/// Binding: a backup of the store carries the index in its snapshot.
#[test]
fn a_backup_carries_the_index_in_its_snapshot() {
    let dir = tmp_dir();
    let dest = tmp_dir();
    let before = {
        let mut db = GraphDb::open(&dir).unwrap();
        seed(&mut db);
        db.snapshot().unwrap();
        db.backup_to(&dest).unwrap();
        observable(&db)
    };
    let db = GraphDb::open(&dest).unwrap();
    assert!(db.fulltext_loaded_from_snapshot());
    assert_eq!(observable(&db), before);
}

/// Binding: a property named twice in one insert is indexed as what is
/// stored — the last value — live, and after a snapshot and a reopen.
///
/// The insert indexed each occurrence on top of the last, so the first
/// value's words stayed searchable on a node that no longer held them. An
/// open used to rebuild the index and heal that; a snapshot now carries the
/// index, so it has to be right when it is written.
#[test]
fn a_field_named_twice_in_one_insert_is_indexed_once() {
    let dir = tmp_dir();
    let never_closed = {
        let mut db = GraphDb::open(&dir).unwrap();
        db.enable_fulltext("Doc", "body").unwrap();
        db.insert_node(
            "Doc",
            "a",
            vec![
                ("body".into(), Value::Str("alpha beta".into())),
                ("body".into(), Value::Str("gamma".into())),
            ],
        )
        .unwrap();
        db.insert_node("Doc", "b", doc("beta gamma", "t")).unwrap();
        assert_eq!(db.get_prop("a", "body"), Some(Value::Str("gamma".into())));
        let hits: Vec<String> = db.search("body", "beta").into_iter().map(|h| h.0).collect();
        assert_eq!(hits, vec!["b"], "a no longer says beta");
        db.snapshot().unwrap();
        (db.search("body", "beta"), db.search("body", "gamma"))
    };
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.fulltext_loaded_from_snapshot());
    assert_eq!(
        (db.search("body", "beta"), db.search("body", "gamma")),
        never_closed
    );
}

/// Binding: a declared field whose every value has no words — an empty
/// string — does not make a loaded index differ from a built one. In a debug
/// build the open compares the two, and it used to panic here on an index
/// that answered every search correctly.
#[test]
fn a_field_with_no_words_in_it_reopens() {
    let dir = tmp_dir();
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.enable_fulltext("Doc", "body").unwrap();
        db.enable_fulltext("Doc", "title").unwrap();
        db.insert_node("Doc", "a", doc("alpha", "")).unwrap();
        db.insert_node("Doc", "c", doc("gone", "")).unwrap();
        db.delete_node("c").unwrap();
        db.snapshot().unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.fulltext_loaded_from_snapshot());
    assert_eq!(db.search("body", "alpha").len(), 1);
    assert!(db.search("title", "alpha").is_empty());
}

/// Binding: pairs declared after the data, then a snapshot that keeps its
/// WAL. Nothing the snapshot holds is indexed a second time at open: this
/// store used to tokenize every replayed insert and then rebuild the index
/// on top, twice the work of an open before the index was carried at all.
#[test]
fn a_keep_wal_snapshot_indexes_nothing_twice_at_open() {
    let dir = tmp_dir();
    let before = {
        let mut db = GraphDb::open(&dir).unwrap();
        for i in 0..10 {
            db.insert_node("Doc", &format!("d{i}"), doc("rust graph database", "notes"))
                .unwrap();
        }
        db.enable_fulltext("Doc", "body").unwrap();
        db.snapshot_with(mushroomdb::SnapshotOptions {
            keep_wal: true,
            ..Default::default()
        })
        .unwrap();
        observable(&db)
    };
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.fulltext_loaded_from_snapshot());
    assert_eq!(db.fulltext_reindexed_during_open(), 0);
    assert_eq!(observable(&db), before);
}

/// Binding: a snapshot that kept its WAL has its index read only when the
/// WAL beside it is the one the snapshot marked. If the WAL has lost its
/// tail, the whole of what is left replays over the snapshot, and an index
/// read from the snapshot beforehand is not what that replay produces: here
/// a replayed insert relabels the node, and the snapshot's posting for the
/// old label's field stayed searchable.
#[test]
fn a_keep_wal_snapshot_whose_wal_lost_its_tail_does_not_keep_its_index() {
    let dir = tmp_dir();
    let cut;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.enable_fulltext("Doc", "body").unwrap();
        db.enable_fulltext("Memo", "title").unwrap();
        db.insert_node(
            "Doc",
            "k",
            vec![("body".into(), Value::Str("alpha".into()))],
        )
        .unwrap();
        cut = std::fs::metadata(dir.join("wal.bin")).unwrap().len();
        db.delete_node("k").unwrap();
        db.insert_node(
            "Memo",
            "k",
            vec![("title".into(), Value::Str("zeta".into()))],
        )
        .unwrap();
        db.snapshot_with(mushroomdb::SnapshotOptions {
            keep_wal: true,
            ..Default::default()
        })
        .unwrap();
    }
    let wal = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("wal.bin"))
        .unwrap();
    wal.set_len(cut).unwrap();
    drop(wal);

    // In a debug build this open used to stop on the check that a kept index
    // equals a rebuilt one.
    let db = GraphDb::open(&dir).unwrap();
    assert!(!db.fulltext_loaded_from_snapshot());
    let label = db.node_info("k").map(|n| n.label);
    if label.as_deref() == Some("Doc") {
        assert!(
            db.search("title", "zeta").is_empty(),
            "k is a Doc again; nothing indexes a Doc's title"
        );
    }
}

// ---------------------------------------------------------------------------
// FT-1: a rewrite removes the old value's words, and nothing else changes
// ---------------------------------------------------------------------------

/// Every query's answers, keys with their scores' bits, in key order: two
/// stores holding the same documents answer identically whatever their ids.
fn answers(db: &GraphDb<core_storage::fs::RealFs>, queries: &[&str]) -> Vec<Vec<(String, u64)>> {
    queries
        .iter()
        .map(|q| {
            let mut r: Vec<(String, u64)> = db
                .search("name", q)
                .into_iter()
                .map(|(k, s)| (k, s.to_bits()))
                .collect();
            r.sort();
            r
        })
        .collect()
}

/// A store built from nothing but the final documents: no rewrite, no removal.
fn fresh_store(
    nodes: &BTreeMap<String, (&'static str, Option<Value>)>,
) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = open();
    db.set_fsync_policy(mushroomdb::FsyncPolicy::Relaxed);
    db.enable_fulltext("A", "name").unwrap();
    db.enable_fulltext("B", "name").unwrap();
    db.write_batch(|batch| {
        for (key, (label, value)) in nodes {
            let props = value
                .iter()
                .map(|v| ("name".to_string(), v.clone()))
                .collect();
            batch.insert_node(label, key, props);
        }
    })
    .unwrap();
    db
}

#[test]
fn randomized_rewrites_answer_like_a_store_built_from_the_final_values() {
    const QUERIES: &[&str] = &[
        "graph",
        "run",
        "rust",
        "gra*",
        "r*",
        "\"graph run\"",
        "rust -db",
        "db OR x",
        "person",
    ];
    let words = [
        "graph", "graphs", "run", "running", "rust", "db", "x", "person",
    ];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |m: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % m
    };
    let text = |next: &mut dyn FnMut(u64) -> u64| -> Value {
        let phrase = |next: &mut dyn FnMut(u64) -> u64| {
            (0..next(5))
                .map(|_| words[next(8) as usize])
                .collect::<Vec<_>>()
                .join(" ")
        };
        match next(5) {
            0 => Value::Int(next(9) as i64),
            1 => Value::List((0..next(3)).map(|_| Value::Str(phrase(next))).collect()),
            _ => Value::Str(phrase(next)),
        }
    };

    let dir = tmp_dir();
    let mut db = GraphDb::open(&dir).unwrap();
    db.set_fsync_policy(mushroomdb::FsyncPolicy::Relaxed);
    db.enable_fulltext("A", "name").unwrap();
    db.enable_fulltext("B", "name").unwrap();
    // Label C carries `name` too, and nothing indexes it.
    let labels = ["A", "B", "C"];
    let mut nodes: BTreeMap<String, (&'static str, Option<Value>)> = BTreeMap::new();
    let mut checked = 0;
    for step in 0..600 {
        let key = format!("k{}", next(40));
        match (nodes.contains_key(&key), next(10)) {
            (false, _) => {
                let label = labels[next(3) as usize];
                let value = (next(4) > 0).then(|| text(&mut next));
                let props = value
                    .iter()
                    .map(|v| ("name".to_string(), v.clone()))
                    .collect();
                db.insert_node(label, &key, props).unwrap();
                nodes.insert(key, (label, value));
            }
            (true, 0) => {
                db.delete_node(&key).unwrap();
                nodes.remove(&key);
            }
            (true, 1) => {
                db.remove_prop(&key, "name").unwrap();
                nodes.get_mut(&key).unwrap().1 = None;
            }
            (true, 2) => {
                // Two rewrites of one node in one batch: the second removes
                // the words the first one wrote.
                let (a, b) = (text(&mut next), text(&mut next));
                db.write_batch(|batch| {
                    batch.set_prop(&key, "name", a.clone());
                    batch.set_prop(&key, "name", b.clone());
                })
                .unwrap();
                nodes.get_mut(&key).unwrap().1 = Some(b);
            }
            (true, 3) => {
                let renamed = format!("r{step}");
                db.rename_node(&key, &renamed).unwrap();
                let node = nodes.remove(&key).unwrap();
                nodes.insert(renamed, node);
            }
            (true, _) => {
                let value = text(&mut next);
                db.set_prop(&key, "name", value.clone()).unwrap();
                nodes.get_mut(&key).unwrap().1 = Some(value);
            }
        }
        // A snapshot puts the old values in the base, where the rewrite reads them.
        if step == 200 || step == 400 {
            db.snapshot().unwrap();
        }
        if step % 25 == 24 {
            assert_eq!(
                answers(&db, QUERIES),
                answers(&fresh_store(&nodes), QUERIES),
                "step {step}"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 24);
    let want = answers(&fresh_store(&nodes), QUERIES);
    assert!(
        want.iter().any(|r| !r.is_empty()),
        "the queries match something"
    );
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(answers(&db, QUERIES), want, "reopened");
}
