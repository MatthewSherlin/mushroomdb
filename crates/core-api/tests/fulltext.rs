/// Full-text-lite API tests.
///
/// Pins the semantics of enable_fulltext, disable_fulltext, and search.
/// Each test documents the stable, observable behavior.
use core_api::{GraphDb, GraphError, Value};
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
/// WAL structure after the workload below:
///   pos 0: EnableFulltext("Article","bio")   ← baseline written by snapshot()
///   pos 1: InsertNode "a1"
///   pos 2: InsertNode "a2"
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
        .unwrap(); // WAL pos 1
        db.insert_node(
            "Article",
            "a2",
            vec![("bio".into(), Value::Str("rust lang".into()))],
        )
        .unwrap(); // WAL pos 2
    }
    // WAL has exactly 3 records (0..=2).
    // At every commit, fulltext must be enabled (baseline is always at pos 0).
    for commit in 0..=2u64 {
        let snap = GraphDb::open_at(&dir, commit).unwrap();
        assert!(
            snap.is_fulltext_enabled("Article", "bio"),
            "fulltext must be enabled at WAL pos={commit}"
        );
    }
    // CommitOutOfRange for pos 3 (only 3 records).
    assert!(matches!(
        GraphDb::open_at(&dir, 3),
        Err(GraphError::CommitOutOfRange { .. })
    ));
    // At pos 2 (all WAL records), the as-of state has:
    //   - fulltext enabled (pos 0 baseline)
    //   - a0 with "rust" (pre-snapshot, loaded from the snapshot base — a
    //     truncating snapshot IS the WAL-head state)
    //   - a1 with "python" (pos 1)
    //   - a2 with "rust lang" (pos 2)
    let snap = GraphDb::open_at(&dir, 2).unwrap();
    assert!(
        snap.is_fulltext_enabled("Article", "bio"),
        "fulltext must be enabled at pos=2"
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
