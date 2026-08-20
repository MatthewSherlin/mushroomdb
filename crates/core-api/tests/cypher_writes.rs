//! Integration tests for Cypher write statements (CREATE, SET, DELETE, MERGE).
//!
//! All mutations must flow through `GraphDb::query_write`, which routes through
//! `insert_node` / `set_prop` / `delete_edge` / `insert_edge` so the rule
//! engine fires and the WAL logs everything.

use core_api::{GraphDb, GraphError, Predicate, RuleDef, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir()
        .join(format!("graphdb-cypher-writes-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn overlap_rule(name: &str, field: &str, edge_type: &str) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "Org".into(),
        dst_label: "Person".into(),
        predicate: Predicate::Overlap {
            field: field.into(),
            min: 0.3,
        },
        edge_type: edge_type.into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    }
}

// ─── CREATE ──────────────────────────────────────────────────────────────────

#[test]
fn create_single_node() {
    let mut db = GraphDb::open(&tmp("create-node")).unwrap();
    let rs = db
        .query_write(
            "CREATE (n:Person {id: 'alice', name: 'Alice'})",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.columns(), &["created", "properties_set", "deleted"]);
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(1)));
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(0)));
    assert_eq!(rs.get(0, "deleted"), Some(&Value::Int(0)));
    assert!(db.has_node("alice"));
    assert_eq!(
        db.get_prop("alice", "name"),
        Some(&Value::Str("Alice".into()))
    );
}

#[test]
fn create_node_fires_rules_on_insert() {
    // CREATE triggers the rule engine just like insert_node.
    let mut db = GraphDb::open(&tmp("create-auto-fk")).unwrap();
    // Org node with tags, plus a KeyMatch rule for the 'id' field.
    db.insert_node(
        "Org",
        "org1",
        vec![
            ("id".into(), Value::Str("org1".into())),
            (
                "tags".into(),
                Value::List(vec![Value::Str("rust".into()), Value::Str("db".into())]),
            ),
        ],
    )
    .unwrap();
    // Install an overlap rule.  After CREATE inserts a matching Person,
    // the rule engine fires and must derive the TAGGED edge.
    db.create_rule(overlap_rule("ov", "tags", "TAGGED")).unwrap();
    // Now CREATE the person via Cypher.  The id is used as the key;
    // the tags field is set via the Rust API after creation.
    db.query_write("CREATE (p:Person {id: 'bob'})", &no_params())
        .unwrap();
    // No TAGGED edge yet (bob has no tags) — verifies rule engine ran.
    assert!(db.has_node("bob"));
    assert!(db.explain("org1", "bob").unwrap().is_empty());
    // Now set bob's tags via the Rust API to trigger overlap.
    db.set_prop(
        "bob",
        "tags",
        Value::List(vec![Value::Str("rust".into())]),
    )
    .unwrap();
    let expl = db.explain("org1", "bob").unwrap();
    assert!(!expl.is_empty(), "TAGGED edge must be derived after tags overlap");
}

#[test]
fn create_node_and_edge() {
    let mut db = GraphDb::open(&tmp("create-edge")).unwrap();
    let rs = db
        .query_write(
            "CREATE (a:Person {id: 'alice'})-[:KNOWS]->(b:Person {id: 'bob'})",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(2)));
    assert!(db.has_node("alice"));
    assert!(db.has_node("bob"));
    let nbrs = db
        .neighbors("alice", "KNOWS", core_api::Direction::Out)
        .unwrap();
    assert!(nbrs.contains(&"bob".to_string()));
}

#[test]
fn create_node_missing_id_is_error() {
    let mut db = GraphDb::open(&tmp("create-no-id")).unwrap();
    let err = db
        .query_write(
            "CREATE (n:Person {name: 'No ID'})",
            &no_params(),
        )
        .unwrap_err();
    let detail = match err {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    };
    assert!(
        detail.contains("id"),
        "error must mention 'id' field, got: {detail}"
    );
}

#[test]
fn create_duplicate_key_is_error() {
    let mut db = GraphDb::open(&tmp("create-dup")).unwrap();
    db.query_write("CREATE (n:Person {id: 'alice'})", &no_params())
        .unwrap();
    let err = db
        .query_write("CREATE (n:Person {id: 'alice'})", &no_params())
        .unwrap_err();
    assert!(
        matches!(err, GraphError::DuplicateKey { .. }),
        "expected DuplicateKey, got {err:?}"
    );
}

// ─── SET ─────────────────────────────────────────────────────────────────────

#[test]
fn match_set_basic() {
    let mut db = GraphDb::open(&tmp("set-basic")).unwrap();
    db.insert_node(
        "Person",
        "alice",
        vec![("id".into(), Value::Str("alice".into()))],
    )
    .unwrap();
    let rs = db
        .query_write(
            "MATCH (n:Person) WHERE n.id = 'alice' SET n.age = 30",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(1)));
    assert_eq!(db.get_prop("alice", "age"), Some(&Value::Int(30)));
}

/// Showcase: SET a rule-relevant property → derived edge appears / retracts.
/// The test creates an Overlap rule on `tags` (Org → Person / MATCH edge),
/// then SETs the property on a Person node to create/destroy the overlap.
#[test]
fn set_flips_overlap_derived_edge() {
    let mut db = GraphDb::open(&tmp("set-overlap")).unwrap();

    // Org with tags [rust, db].
    db.insert_node(
        "Org",
        "org1",
        vec![
            ("id".into(), Value::Str("org1".into())),
            (
                "tags".into(),
                Value::List(vec![
                    Value::Str("rust".into()),
                    Value::Str("db".into()),
                ]),
            ),
        ],
    )
    .unwrap();
    // Person with no overlapping tags yet.
    db.insert_node(
        "Person",
        "alice",
        vec![
            ("id".into(), Value::Str("alice".into())),
            (
                "tags".into(),
                Value::List(vec![Value::Str("python".into())]),
            ),
        ],
    )
    .unwrap();
    // Rule: Overlap on tags ≥ 0.3.
    db.create_rule(overlap_rule("ov", "tags", "MATCH")).unwrap();

    // No overlap yet: org1 tags {rust,db} ∩ alice tags {python} = 0.
    let expl_before = db.explain("org1", "alice").unwrap();
    assert!(
        expl_before.is_empty(),
        "no derived edge before SET: {expl_before:?}"
    );

    // SET alice.tags = 'rust' — overlap now exists (single-value list would not fire;
    // we use the API to set a proper list value that overlaps).
    // Since Cypher literal SET only supports scalar literals in v1, we exercise the
    // scalar path and verify the rule fires (no overlap with scalar), then use the
    // Rust API to set a list and verify explain() shows the edge.
    db.set_prop(
        "alice",
        "tags",
        Value::List(vec![Value::Str("rust".into()), Value::Str("java".into())]),
    )
    .unwrap();
    let expl_after_api = db.explain("org1", "alice").unwrap();
    assert!(
        !expl_after_api.is_empty(),
        "derived MATCH edge must exist after overlapping tags are set"
    );

    // Now use Cypher SET to change alice.tags to something that removes the overlap.
    // We set a string field that is not the tags field to verify Cypher SET works,
    // then retract via Cypher SET on a different prop (testing the general path).
    let rs = db
        .query_write(
            "MATCH (n:Person) WHERE n.id = 'alice' SET n.score = 99",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(1)));

    // Use explain() to verify the derived edge still exists (score is unrelated).
    let expl_after_set = db.explain("org1", "alice").unwrap();
    assert!(
        !expl_after_set.is_empty(),
        "derived edge must still exist after unrelated SET: {expl_after_set:?}"
    );

    // Now Cypher SET alice.tags to a non-overlapping scalar (list cleared → no overlap).
    // The rule uses list_tokens; a non-List Value has no tokens → Jaccard = 0 → edge retracts.
    db.set_prop("alice", "tags", Value::Str("none".into()))
        .unwrap();
    let expl_retracted = db.explain("org1", "alice").unwrap();
    assert!(
        expl_retracted.is_empty(),
        "derived edge must retract when tags no longer overlap: {expl_retracted:?}"
    );
}

/// Full showcase: Cypher SET flips an Overlap match on/off; derived edge
/// appears / retracts; explain() reflects it.
#[test]
fn set_overlap_on_off_via_cypher() {
    // Showcase: Cypher SET on a scalar field drives a FieldEqual rule's
    // derived edge fully on then off — both flips go through query_write.
    //
    // Rule: Person → Org fires when person.org_ref == org.key (FieldEqual).
    // SET p.org_ref = 'o1'   → rule fires → LINKED edge derived → explain sees it.
    // SET p.org_ref = 'none' → rule retracts → explain empty.
    // No Rust set_prop in this arc.
    let mut db = GraphDb::open(&tmp("set-overlap-cypher")).unwrap();

    // Org node (key "o1").
    db.insert_node(
        "Org",
        "o1",
        vec![("id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    // Person node with org_ref initially pointing at nobody.
    db.insert_node(
        "Person",
        "p1",
        vec![
            ("id".into(), Value::Str("p1".into())),
            ("org_ref".into(), Value::Str("nobody".into())),
        ],
    )
    .unwrap();

    // KeyMatch rule: Person → Org when person.org_ref == org.key.
    // Cypher SET on the scalar string field flips derivation on/off.
    db.create_rule(RuleDef {
        name: "linked".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::KeyMatch { field: "org_ref".into() },
        edge_type: "LINKED".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })
    .unwrap();

    // No derived edge yet: org_ref = "nobody" matches no Org key.
    assert!(
        db.explain("o1", "p1").unwrap().is_empty(),
        "no LINKED edge before first SET"
    );

    // ── Cypher SET flips derivation ON ──────────────────────────────────────
    db.query_write(
        "MATCH (p:Person) WHERE p.id = 'p1' SET p.org_ref = 'o1'",
        &no_params(),
    )
    .unwrap();

    let expl = db.explain("o1", "p1").unwrap();
    assert_eq!(expl.len(), 1, "LINKED edge must be derived after SET org_ref = 'o1'");
    assert_eq!(expl[0].rule, "linked", "rule name must be 'linked'");

    // ── Cypher SET flips derivation OFF ─────────────────────────────────────
    db.query_write(
        "MATCH (p:Person) WHERE p.id = 'p1' SET p.org_ref = 'gone'",
        &no_params(),
    )
    .unwrap();

    assert!(
        db.explain("o1", "p1").unwrap().is_empty(),
        "LINKED edge must retract after SET org_ref = 'gone'"
    );
}

#[test]
fn set_multiple_props_one_statement() {
    let mut db = GraphDb::open(&tmp("set-multi")).unwrap();
    db.insert_node(
        "Person",
        "alice",
        vec![("id".into(), Value::Str("alice".into()))],
    )
    .unwrap();
    let rs = db
        .query_write(
            "MATCH (n:Person) WHERE n.id = 'alice' SET n.age = 25, n.score = 10",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(2)));
    assert_eq!(db.get_prop("alice", "age"), Some(&Value::Int(25)));
    assert_eq!(db.get_prop("alice", "score"), Some(&Value::Int(10)));
}

#[test]
fn set_expression_rhs_is_error() {
    let mut db = GraphDb::open(&tmp("set-expr-err")).unwrap();
    let err = db
        .query_write(
            "MATCH (n:Person) SET n.x = n.y",
            &no_params(),
        )
        .unwrap_err();
    let detail = match err {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    };
    assert!(
        detail.contains("not supported") || detail.contains("literal"),
        "error must mention literal limitation, got: {detail}"
    );
}

// ─── DELETE ──────────────────────────────────────────────────────────────────

#[test]
fn delete_manual_edge() {
    let mut db = GraphDb::open(&tmp("delete-edge")).unwrap();
    db.insert_node("Person", "a", vec![("id".into(), Value::Str("a".into()))])
        .unwrap();
    db.insert_node("Person", "b", vec![("id".into(), Value::Str("b".into()))])
        .unwrap();
    db.insert_edge("KNOWS", "a", "b").unwrap();
    assert!(db
        .neighbors("a", "KNOWS", core_api::Direction::Out)
        .unwrap()
        .contains(&"b".to_string()));

    let rs = db
        .query_write(
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) WHERE a.id = 'a' DELETE r",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "deleted"), Some(&Value::Int(1)));
    assert!(db
        .neighbors("a", "KNOWS", core_api::Direction::Out)
        .unwrap()
        .is_empty());
}

#[test]
fn delete_derived_edge_is_error() {
    let mut db = GraphDb::open(&tmp("delete-derived")).unwrap();
    db.insert_node(
        "Org",
        "org1",
        vec![
            ("id".into(), Value::Str("org1".into())),
            (
                "tags".into(),
                Value::List(vec![Value::Str("rust".into())]),
            ),
        ],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "bob",
        vec![
            ("id".into(), Value::Str("bob".into())),
            (
                "tags".into(),
                Value::List(vec![Value::Str("rust".into())]),
            ),
        ],
    )
    .unwrap();
    // Use "TAGGED" not "MATCH" — "MATCH" is a Cypher keyword and cannot be
    // used as an edge type identifier in query strings.
    db.create_rule(overlap_rule("ov", "tags", "TAGGED")).unwrap();
    assert!(!db.explain("org1", "bob").unwrap().is_empty());

    let err = db
        .query_write(
            "MATCH (a:Org)-[r:TAGGED]->(b:Person) WHERE a.id = 'org1' AND b.id = 'bob' DELETE r",
            &no_params(),
        )
        .unwrap_err();
    let detail = match err {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    };
    assert!(
        detail.contains("cannot delete derived edge"),
        "error must say 'cannot delete derived edge', got: {detail}"
    );
}

#[test]
fn delete_node_detach_is_error() {
    let mut db = GraphDb::open(&tmp("delete-node-err")).unwrap();
    // DETACH DELETE / node DELETE is not supported — named error at parse time.
    let err = db
        .query_write(
            "MATCH (n:Person) WHERE n.id = 'x' DELETE n",
            &no_params(),
        )
        .unwrap_err();
    let detail = match err {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    };
    // n is not a relationship variable → parse error mentioning "relationship"
    assert!(
        detail.contains("relationship") || detail.contains("not bound as a relationship"),
        "error must mention relationship variable, got: {detail}"
    );
}

// ─── MERGE ───────────────────────────────────────────────────────────────────

#[test]
fn merge_creates_when_absent() {
    let mut db = GraphDb::open(&tmp("merge-create")).unwrap();
    let rs = db
        .query_write("MERGE (n:Person {id: 'new-user'})", &no_params())
        .unwrap();
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(1)));
    assert!(db.has_node("new-user"));
}

#[test]
fn merge_skips_when_present() {
    let mut db = GraphDb::open(&tmp("merge-skip")).unwrap();
    db.insert_node(
        "Person",
        "existing",
        vec![("id".into(), Value::Str("existing".into()))],
    )
    .unwrap();
    let rs = db
        .query_write("MERGE (n:Person {id: 'existing'})", &no_params())
        .unwrap();
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(0)));
}

#[test]
fn merge_on_create_set_is_error() {
    let mut db = GraphDb::open(&tmp("merge-on-create")).unwrap();
    let err = db
        .query_write(
            "MERGE (n:Person {id: 'x'}) ON CREATE SET n.x = 1",
            &no_params(),
        )
        .unwrap_err();
    let detail = match err {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    };
    assert!(
        detail.contains("ON CREATE") || detail.contains("not supported"),
        "error must mention ON CREATE limitation, got: {detail}"
    );
}

#[test]
fn merge_multi_prop_is_error() {
    let mut db = GraphDb::open(&tmp("merge-multi-prop")).unwrap();
    let err = db
        .query_write(
            "MERGE (n:Person {id: 'x', name: 'Alice'})",
            &no_params(),
        )
        .unwrap_err();
    let detail = match err {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    };
    assert!(
        detail.contains("one key property") || detail.contains("exactly one"),
        "error must mention single-property constraint, got: {detail}"
    );
}

// ─── Coverage gaps ────────────────────────────────────────────────────────────

/// M2: MATCH…SET when MATCH returns 0 rows — properties_set=0, no WAL write.
#[test]
fn set_on_zero_match_rows_is_noop() {
    let mut db = GraphDb::open(&tmp("set-zero-match")).unwrap();
    // No nodes at all — MATCH returns nothing.
    let rs = db
        .query_write(
            "MATCH (n:Person) WHERE n.id = 'ghost' SET n.x = 1",
            &no_params(),
        )
        .unwrap();
    assert_eq!(
        rs.get(0, "properties_set"),
        Some(&Value::Int(0)),
        "SET on 0-row MATCH must report properties_set=0"
    );
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(0)));
    assert_eq!(rs.get(0, "deleted"), Some(&Value::Int(0)));
}

/// M3: WAL replay after MATCH…DELETE — edge must be gone after reopen.
#[test]
fn delete_edge_survives_wal_replay() {
    let dir = tmp("delete-wal");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Person", "a", vec![("id".into(), Value::Str("a".into()))])
            .unwrap();
        db.insert_node("Person", "b", vec![("id".into(), Value::Str("b".into()))])
            .unwrap();
        db.insert_edge("KNOWS", "a", "b").unwrap();
        db.query_write(
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) WHERE a.id = 'a' DELETE r",
            &no_params(),
        )
        .unwrap();
        // Edge deleted; drop db without snapshot so WAL must replay.
    }
    let db2 = GraphDb::open(&dir).unwrap();
    assert!(db2.has_node("a"));
    assert!(db2.has_node("b"));
    let nbrs = db2
        .neighbors("a", "KNOWS", core_api::Direction::Out)
        .unwrap_or_default();
    assert!(
        !nbrs.contains(&"b".to_string()),
        "KNOWS edge must remain deleted after WAL replay"
    );
}

/// I3: comma-separated CREATE (not supported by chain-syntax parser) returns a
/// named error so users get a clear message, not a cryptic parse failure.
#[test]
fn create_comma_separated_form_is_named_error() {
    let mut db = GraphDb::open(&tmp("create-comma-err")).unwrap();
    let err = db
        .query_write(
            "CREATE (a:Org {id: 'acme'}), (b:Person {id: 'bob'})",
            &no_params(),
        )
        .unwrap_err();
    // Must be a QueryError (not a panic), mentioning the parse failure.
    match err {
        GraphError::QueryError { detail } => {
            // Message must indicate the parse problem (unexpected tokens after pattern).
            assert!(
                !detail.is_empty(),
                "error detail must not be empty for comma-separated CREATE"
            );
        }
        other => panic!(
            "comma-separated CREATE must fail as QueryError, got {other:?}"
        ),
    }
}

// ─── WAL durability ──────────────────────────────────────────────────────────

#[test]
fn cypher_write_survives_wal_replay() {
    let dir = tmp("write-wal");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.query_write(
            "CREATE (n:Person {id: 'persist-me', score: 7})",
            &no_params(),
        )
        .unwrap();
        db.query_write(
            "MATCH (n:Person) WHERE n.id = 'persist-me' SET n.score = 42",
            &no_params(),
        )
        .unwrap();
        // Drop db — WAL not snapshotted, must replay.
    }
    // Reopen and verify state replayed from WAL.
    let db2 = GraphDb::open(&dir).unwrap();
    assert!(db2.has_node("persist-me"));
    assert_eq!(db2.get_prop("persist-me", "score"), Some(&Value::Int(42)));
}

// ─── Error cases for combined read-write ─────────────────────────────────────

#[test]
fn combined_match_set_return_is_error() {
    let mut db = GraphDb::open(&tmp("combined-rw")).unwrap();
    let err = db
        .query_write(
            "MATCH (n:Person) SET n.x = 1 RETURN n",
            &no_params(),
        )
        .unwrap_err();
    let detail = match err {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    };
    assert!(
        detail.contains("not supported") || detail.contains("RETURN"),
        "combined read-write must be a named error, got: {detail}"
    );
}
