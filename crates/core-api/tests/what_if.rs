//! `what_if_set_prop` — the derived edges a prop change would add or remove,
//! without committing anything.

use core_api::{GraphDb, GraphError, Predicate, RuleDef, Value};
use std::collections::BTreeSet;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-whatif-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn teammates_rule() -> RuleDef {
    RuleDef {
        name: "teammates".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::FieldEqual {
            field: "team".into(),
        },
        edge_type: "Teammate".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

fn seed(dir: &std::path::Path) {
    let mut db = GraphDb::open(dir).unwrap();
    for (k, team) in [
        ("a", "red"),
        ("b", "red"),
        ("c", "blue"),
        ("d", "blue"),
        ("e", "green"),
    ] {
        db.insert_node("Person", k, vec![("team".into(), Value::Str(team.into()))])
            .unwrap();
    }
    db.insert_edge("Knows", "a", "e").unwrap();
    db.create_rule(teammates_rule()).unwrap();
    drop(db);
}

/// Copy a store directory (flat — no subdirectories in a mushroomdb store).
fn copy_store(src: &std::path::Path, dst: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dst);
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), dst.join(entry.file_name())).unwrap();
        }
    }
}

/// (edge_type, src, dst) of one edge.
type Triple = (String, String, String);

/// Derived edges incident to `key`, as (edge_type, src, dst).
fn derived_edges(db: &GraphDb<core_storage::fs::RealFs>, key: &str) -> BTreeSet<Triple> {
    db.node_edges(key)
        .unwrap()
        .into_iter()
        .filter(|e| e.derived)
        .map(|e| (e.edge_type, e.src_key, e.dst_key))
        .collect()
}

/// The oracle: apply a real `set_prop` on a scratch copy and diff.
fn real_diff(
    dir: &std::path::Path,
    key: &str,
    field: &str,
    value: Value,
) -> (BTreeSet<Triple>, BTreeSet<Triple>) {
    let scratch = dir.with_extension("scratch");
    copy_store(dir, &scratch);
    let mut db = GraphDb::open(&scratch).unwrap();
    let before = derived_edges(&db, key);
    db.set_prop(key, field, value).unwrap();
    let after = derived_edges(&db, key);
    drop(db);
    let _ = std::fs::remove_dir_all(&scratch);
    (
        before.difference(&after).cloned().collect(),
        after.difference(&before).cloned().collect(),
    )
}

fn as_triples(edges: &[core_api::EdgeAt]) -> BTreeSet<Triple> {
    edges
        .iter()
        .map(|e| (e.edge_type.clone(), e.src_key.clone(), e.dst_key.clone()))
        .collect()
}

#[test]
fn what_if_set_prop_matches_a_real_set_prop() {
    let dir = tmp("match");
    seed(&dir);

    for (key, field, value) in [
        ("c", "team", Value::Str("red".into())),
        ("a", "team", Value::Str("green".into())),
        ("e", "team", Value::Str("blue".into())),
        ("a", "team", Value::Str("purple".into())),
    ] {
        let db = GraphDb::open(&dir).unwrap();
        let wi = db.what_if_set_prop(key, field, value.clone()).unwrap();
        drop(db);

        let (want_lost, want_gained) = real_diff(&dir, key, field, value.clone());

        assert_eq!(
            as_triples(&wi.lost),
            want_lost,
            "lost mismatch for set_prop({key}, {field}, {value:?})"
        );
        assert_eq!(
            as_triples(&wi.gained),
            want_gained,
            "gained mismatch for set_prop({key}, {field}, {value:?})"
        );
        for e in wi.lost.iter().chain(wi.gained.iter()) {
            assert!(e.derived, "what_if reported a non-derived edge: {e:?}");
            assert_eq!(e.rule.as_deref(), Some("teammates"), "{e:?}");
        }
    }
}

#[test]
fn what_if_set_prop_does_not_mutate_the_store() {
    let dir = tmp("pure");
    seed(&dir);

    let mut db = GraphDb::open(&dir).unwrap();
    let commits_before = db.wal_total_commits().unwrap();
    let edges_before = db.edge_count();
    let derived_before = derived_edges(&db, "c");
    let fires_before: Vec<u64> = db.stats().rules.iter().map(|r| r.edges).collect();

    let wi = db
        .what_if_set_prop("c", "team", Value::Str("red".into()))
        .unwrap();
    assert!(!wi.gained.is_empty(), "expected new Teammate edges for c");

    assert_eq!(db.wal_total_commits().unwrap(), commits_before, "WAL grew");
    assert_eq!(db.edge_count(), edges_before, "edge count changed");
    assert_eq!(
        derived_edges(&db, "c"),
        derived_before,
        "derived edges changed"
    );
    assert_eq!(
        db.stats()
            .rules
            .iter()
            .map(|r| r.edges)
            .collect::<Vec<u64>>(),
        fires_before,
        "provenance size changed"
    );

    // A real set_prop afterwards still produces exactly what what_if promised.
    db.set_prop("c", "team", Value::Str("red".into())).unwrap();
    let after = derived_edges(&db, "c");
    let gained: BTreeSet<_> = after.difference(&derived_before).cloned().collect();
    assert_eq!(as_triples(&wi.gained), gained);
}

#[test]
fn what_if_set_prop_on_a_read_only_handle_works() {
    let dir = tmp("ro");
    seed(&dir);
    let db = GraphDb::open_with_options(
        &dir,
        core_api::OpenOptions {
            auto_migrate: false,
            repair_wal: false,
            read_only: true,
        },
    )
    .unwrap();
    let wi = db
        .what_if_set_prop("c", "team", Value::Str("red".into()))
        .unwrap();
    assert!(!wi.gained.is_empty(), "expected gained edges, got {wi:?}");
}

#[test]
fn what_if_set_prop_unknown_key_errors() {
    let dir = tmp("unknown");
    seed(&dir);
    let db = GraphDb::open(&dir).unwrap();
    match db.what_if_set_prop("nope", "team", Value::Str("red".into())) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "nope"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}

#[test]
fn what_if_set_prop_with_no_effect_is_empty() {
    let dir = tmp("noop");
    seed(&dir);
    let db = GraphDb::open(&dir).unwrap();
    // Same value it already has.
    let wi = db
        .what_if_set_prop("a", "team", Value::Str("red".into()))
        .unwrap();
    assert!(wi.lost.is_empty() && wi.gained.is_empty(), "{wi:?}");
    // A field no rule watches.
    let wi = db
        .what_if_set_prop("a", "nickname", Value::Str("ace".into()))
        .unwrap();
    assert!(wi.lost.is_empty() && wi.gained.is_empty(), "{wi:?}");
}
