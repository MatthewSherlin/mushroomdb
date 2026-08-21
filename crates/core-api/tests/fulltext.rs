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
    let d = std::env::temp_dir()
        .join(format!("graphdb-ft-{}-{}", n, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    GraphDb::open(&d).unwrap()
}

fn tmp_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(10000);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir()
        .join(format!("graphdb-ft-dir-{}-{}", n, std::process::id()));
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
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("hello world rust".into()))]).unwrap();
    db.insert_node("Doc", "d1", vec![("body".into(), Value::Str("hello python".into()))]).unwrap();

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
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("rust programming".into()))]).unwrap();
    db.insert_node("Doc", "d1", vec![("body".into(), Value::Str("python scripting".into()))]).unwrap();
    db.insert_node("Doc", "d2", vec![("body".into(), Value::Str("javascript web".into()))]).unwrap();

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
    db.insert_node("A", "a0", vec![("f".into(), Value::Str("alpha beta gamma".into()))]).unwrap();
    db.insert_node("A", "a1", vec![("f".into(), Value::Str("alpha only".into()))]).unwrap();

    // "alpha AND beta" same as "alpha beta"
    let r = db.search("f", "alpha AND beta");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "a0");
}

#[test]
fn prefix_match() {
    let mut db = open();
    db.enable_fulltext("Doc", "title").unwrap();
    db.insert_node("Doc", "d0", vec![("title".into(), Value::Str("rustlang".into()))]).unwrap();
    db.insert_node("Doc", "d1", vec![("title".into(), Value::Str("rusty nails".into()))]).unwrap();
    db.insert_node("Doc", "d2", vec![("title".into(), Value::Str("python".into()))]).unwrap();

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
    db.insert_node("X", "n0", vec![("f".into(), Value::Str("rustlang awesome".into()))]).unwrap();
    db.insert_node("X", "n1", vec![("f".into(), Value::Str("rustlang boring".into()))]).unwrap();
    db.insert_node("X", "n2", vec![("f".into(), Value::Str("python awesome".into()))]).unwrap();

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
    db.insert_node("Msg", "m0", vec![("text".into(), Value::Str("Hello World RUST".into()))]).unwrap();

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
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("rust rocks".into()))]).unwrap();

    // Initially finds "rust"
    assert_eq!(db.search("body", "rust").len(), 1);
    assert_eq!(db.search("body", "python").len(), 0);

    // Update to python — old tokens gone, new present
    db.set_prop("d0", "body", Value::Str("python rules".into())).unwrap();

    assert_eq!(db.search("body", "rust").len(), 0, "old token must be gone");
    assert_eq!(db.search("body", "python").len(), 1, "new token must be found");
}

// ---------------------------------------------------------------------------
// Delete removes node from index
// ---------------------------------------------------------------------------

#[test]
fn delete_node_removes_from_index() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("rust programming".into()))]).unwrap();
    db.insert_node("Doc", "d1", vec![("body".into(), Value::Str("rust databases".into()))]).unwrap();

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
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("hello rust".into()))]).unwrap();

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
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("hello rust".into()))]).unwrap();

    // Stable pinned behavior: empty result, no error
    let r = db.search("body", "hello");
    assert!(r.is_empty(), "unindexed field must return empty, got: {r:?}");
}

#[test]
fn search_wrong_field_returns_empty() {
    let mut db = open();
    db.enable_fulltext("Doc", "bio").unwrap();
    db.insert_node("Doc", "d0", vec![("bio".into(), Value::Str("hello".into()))]).unwrap();

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
        db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("rust is great".into()))]).unwrap();
        db.insert_node("Doc", "d1", vec![("body".into(), Value::Str("python django".into()))]).unwrap();
        db.set_prop("d0", "body", Value::Str("rust and databases".into())).unwrap();
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
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("hello rust".into()))]).unwrap();

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
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("rust rocks".into()))]).unwrap();
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
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("hello rust".into()))]).unwrap();
    db.insert_node("Article", "a0", vec![("body".into(), Value::Str("hello rust".into()))]).unwrap();

    let r = db.search("body", "hello");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "d0"); // only Doc, not Article
}

// ---------------------------------------------------------------------------
// Ranking by match_count descending
// ---------------------------------------------------------------------------

#[test]
fn ranking_match_count_desc() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    // d0 matches both OR-groups (alpha + beta); d1 matches only one (beta)
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("alpha beta".into()))]).unwrap();
    db.insert_node("Doc", "d1", vec![("body".into(), Value::Str("beta gamma".into()))]).unwrap();

    let r = db.search("body", "alpha OR beta");
    assert_eq!(r.len(), 2);
    // d0 has match_count=2 (matches both "alpha" group AND "beta" group)
    assert_eq!(r[0].0, "d0");
    assert_eq!(r[0].1, 2);
    assert_eq!(r[1].0, "d1");
    assert_eq!(r[1].1, 1);
}

// ---------------------------------------------------------------------------
// Oracle equivalence: db.search == db.scratch_search
// ---------------------------------------------------------------------------

#[test]
fn oracle_equivalence_basic() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("I love Rust and databases".into()))]).unwrap();
    db.insert_node("Doc", "d1", vec![("body".into(), Value::Str("Python is also great".into()))]).unwrap();
    db.insert_node("Doc", "d2", vec![("body".into(), Value::Str("rust AND python both useful".into()))]).unwrap();

    for q in &["rust", "python", "rust OR python", "rust*", "databases", "nope"] {
        let idx = db.search("body", q);
        let scratch = db.scratch_search("body", q);
        assert_eq!(idx, scratch, "oracle mismatch for query {q:?}");
    }
}

#[test]
fn oracle_after_update_and_delete() {
    let mut db = open();
    db.enable_fulltext("Doc", "body").unwrap();
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("rust rocks".into()))]).unwrap();
    db.insert_node("Doc", "d1", vec![("body".into(), Value::Str("python cool".into()))]).unwrap();
    db.insert_node("Doc", "d2", vec![("body".into(), Value::Str("java enterprise".into()))]).unwrap();

    db.set_prop("d0", "body", Value::Str("python now".into())).unwrap();
    db.delete_node("d2").unwrap();

    for q in &["rust", "python", "java", "rust OR python", "now", "py*"] {
        let idx = db.search("body", q);
        let scratch = db.scratch_search("body", q);
        assert_eq!(idx, scratch, "oracle mismatch for query {q:?}");
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
        db.insert_node("Article", "a0", vec![("bio".into(), Value::Str("rust embedded graph".into()))]).unwrap();
        db.insert_node("Article", "a1", vec![("bio".into(), Value::Str("python scripting".into()))]).unwrap();
        db.snapshot().unwrap();
        // Confirm search works before close.
        let r = db.search("bio", "rust");
        assert_eq!(r.len(), 1, "pre-close search must find a0");
    }
    // Reopen from snapshot.
    {
        let db2 = GraphDb::open(&dir).unwrap();
        assert!(db2.is_fulltext_enabled("Article", "bio"), "declaration must survive snapshot");
        let r = db2.search("bio", "rust");
        assert_eq!(r.len(), 1, "index must be rebuilt after reopen from snapshot");
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
        db.insert_node("Article", "a0", vec![("bio".into(), Value::Str("rust".into()))]).unwrap();
        db.snapshot().unwrap();
        // snapshot() replaces WAL with baseline: [EnableFulltext("Article","bio")]
        db.insert_node("Article", "a1", vec![("bio".into(), Value::Str("python".into()))]).unwrap(); // WAL pos 1
        db.insert_node("Article", "a2", vec![("bio".into(), Value::Str("rust lang".into()))]).unwrap(); // WAL pos 2
    }
    // WAL has exactly 3 records (0..=2).
    // At every commit, fulltext must be enabled (baseline is always at pos 0).
    for commit in 0..=2u64 {
        let snap = GraphDb::open_at(&dir, commit).unwrap();
        assert!(snap.is_fulltext_enabled("Article", "bio"),
            "fulltext must be enabled at WAL pos={commit}");
    }
    // CommitOutOfRange for pos 3 (only 3 records).
    assert!(matches!(GraphDb::open_at(&dir, 3), Err(GraphError::CommitOutOfRange { .. })));
    // At pos 2 (all WAL records), the WAL-replayed state has:
    //   - fulltext enabled (pos 0 baseline)
    //   - a1 with "python" (pos 1)
    //   - a2 with "rust lang" (pos 2)
    // open_at is WAL-only (no snapshot), so a0 (pre-snapshot) is not visible.
    let snap = GraphDb::open_at(&dir, 2).unwrap();
    assert!(snap.is_fulltext_enabled("Article", "bio"), "fulltext must be enabled at pos=2");
    let r = snap.search("bio", "rust");
    assert_eq!(r.len(), 1, "only a2 should match 'rust' (a0 is pre-snapshot, not in WAL)");
    assert_eq!(r[0].0, "a2");
}

// ---------------------------------------------------------------------------
// Cypher textMatches function in WHERE position
// ---------------------------------------------------------------------------

#[test]
fn cypher_text_matches_where() {
    let mut db = open();
    db.insert_node("Doc", "d0", vec![("body".into(), Value::Str("rust is great".into()))]).unwrap();
    db.insert_node("Doc", "d1", vec![("body".into(), Value::Str("python scripting".into()))]).unwrap();

    let no_params: BTreeMap<String, Value> = BTreeMap::new();

    // textMatches does per-row scratch matching — no index needed
    let rs = db
        .query("MATCH (d:Doc) WHERE textMatches(d.body, 'rust') RETURN d", &no_params)
        .unwrap();
    assert_eq!(rs.len(), 1);

    let rs2 = db
        .query("MATCH (d:Doc) WHERE textMatches(d.body, 'python OR rust') RETURN d", &no_params)
        .unwrap();
    assert_eq!(rs2.len(), 2);

    let rs3 = db
        .query("MATCH (d:Doc) WHERE textMatches(d.body, 'rust*') RETURN d", &no_params)
        .unwrap();
    assert_eq!(rs3.len(), 1);
}
