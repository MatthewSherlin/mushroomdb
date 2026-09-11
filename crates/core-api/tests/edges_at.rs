//! `edges_at` — every edge incident to a node at one WAL commit, from one scan.

use core_api::{GraphDb, GraphError, OpenOptions, Predicate, RuleDef, Value};
use std::collections::BTreeSet;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-edgesat-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Seed a store whose edges appear and disappear: manual inserts/deletes, a
/// rule that derives and retracts, and a node deletion.
fn seed(dir: &std::path::Path) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    db.insert_node(
        "Person",
        "a",
        vec![("team".into(), Value::Str("red".into()))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "b",
        vec![("team".into(), Value::Str("red".into()))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "c",
        vec![("team".into(), Value::Str("blue".into()))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "d",
        vec![("team".into(), Value::Str("blue".into()))],
    )
    .unwrap();

    db.insert_edge("Knows", "a", "b").unwrap();
    db.insert_edge("Knows", "a", "c").unwrap();
    db.insert_edge("Likes", "b", "a").unwrap();
    db.delete_edge("Knows", "a", "c").unwrap();
    db.insert_edge("Knows", "c", "d").unwrap();

    // A rule derives Teammate between same-team Persons.
    db.create_rule(RuleDef {
        name: "teammates".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        edge_type: "Teammate".into(),
        predicate: Predicate::FieldEqual {
            field: "team".into(),
        },
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();

    // Moving c onto the red team retracts c↔d and derives c↔a, c↔b.
    db.set_prop("c", "team", Value::Str("red".into())).unwrap();
    // Moving a off the red team retracts every Teammate a had.
    db.set_prop("a", "team", Value::Str("green".into()))
        .unwrap();
    db.insert_edge("Knows", "d", "a").unwrap();
    db.delete_node("d").unwrap();
    db
}

/// Every node key that ever appears, for the cross-product oracle.
const KEYS: [&str; 4] = ["a", "b", "c", "d"];
const ETYPES: [&str; 3] = ["Knows", "Likes", "Teammate"];

#[test]
fn edges_at_agrees_with_was_linked_at_every_commit() {
    let dir = tmp("agree");
    let db = seed(&dir);
    let total = db.wal_total_commits().unwrap();
    assert!(total > 8, "seed should produce a real history, got {total}");

    for commit in db.wal_horizon_floor()..total {
        for k in KEYS {
            let at = db.edges_at(k, commit).unwrap();
            // Sorted by (edge_type, src, dst).
            let sorted: Vec<_> = {
                let mut v: Vec<_> = at
                    .iter()
                    .map(|e| (e.edge_type.clone(), e.src_key.clone(), e.dst_key.clone()))
                    .collect();
                v.sort();
                v
            };
            let asis: Vec<_> = at
                .iter()
                .map(|e| (e.edge_type.clone(), e.src_key.clone(), e.dst_key.clone()))
                .collect();
            assert_eq!(asis, sorted, "edges_at({k}, {commit}) not sorted");

            // Every returned edge must be incident to k.
            for e in &at {
                assert!(
                    e.src_key == k || e.dst_key == k,
                    "edges_at({k}, {commit}) returned non-incident {e:?}"
                );
            }

            let have: BTreeSet<(String, String)> = at
                .iter()
                .map(|e| {
                    let other = if e.src_key == k {
                        e.dst_key.clone()
                    } else {
                        e.src_key.clone()
                    };
                    (e.edge_type.clone(), other)
                })
                .collect();

            for other in KEYS {
                if other == k {
                    continue;
                }
                for et in ETYPES {
                    let want = db.was_linked(k, other, et, commit).unwrap();
                    let got = have.contains(&(et.to_string(), other.to_string()));
                    assert_eq!(
                        got, want,
                        "commit {commit}: edges_at({k}) vs was_linked({k},{other},{et}) — {at:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn edges_at_marks_derived_edges_with_their_rule() {
    let dir = tmp("derived");
    let db = seed(&dir);
    let last = db.wal_total_commits().unwrap() - 1;

    // At the end of the seeded history b and c are both on the red team.
    let at = db.edges_at("b", last).unwrap();
    let teammate: Vec<_> = at.iter().filter(|e| e.edge_type == "Teammate").collect();
    assert!(
        !teammate.is_empty(),
        "expected a derived Teammate edge on b, got {at:?}"
    );
    for e in teammate {
        assert!(e.derived, "derived flag not set: {e:?}");
        assert_eq!(e.rule.as_deref(), Some("teammates"), "{e:?}");
    }
    for e in at.iter().filter(|e| e.edge_type != "Teammate") {
        assert!(!e.derived, "manual edge flagged derived: {e:?}");
        assert_eq!(e.rule, None, "{e:?}");
    }
}

#[test]
fn edges_at_reports_a_renamed_nodes_edges_under_the_current_key() {
    let dir = tmp("rename");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "old", vec![]).unwrap();
    db.insert_node("Person", "p", vec![]).unwrap();
    db.insert_edge("Knows", "old", "p").unwrap();
    let before = db.wal_total_commits().unwrap() - 1;
    db.rename_node("old", "new").unwrap();
    let after = db.wal_total_commits().unwrap() - 1;

    // Queried under the current key, the edge written under the old key shows up.
    let at = db.edges_at("new", before).unwrap();
    assert_eq!(at.len(), 1, "{at:?}");
    assert_eq!(at[0].edge_type, "Knows");
    assert_eq!(at[0].src_key, "new", "endpoint not canonicalised: {at:?}");
    assert_eq!(at[0].dst_key, "p");

    let at = db.edges_at("new", after).unwrap();
    assert_eq!(at.len(), 1, "{at:?}");
    assert_eq!(at[0].src_key, "new");

    // The partner's view names the renamed node by its current key too.
    let at = db.edges_at("p", after).unwrap();
    assert_eq!(at.len(), 1, "{at:?}");
    assert_eq!(at[0].src_key, "new", "{at:?}");
}

#[test]
fn edges_at_is_empty_after_the_node_is_deleted() {
    let dir = tmp("deleted");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "a", vec![]).unwrap();
    db.insert_node("Person", "b", vec![]).unwrap();
    db.insert_edge("Knows", "a", "b").unwrap();
    let linked = db.wal_total_commits().unwrap() - 1;
    db.delete_node("a").unwrap();
    let gone = db.wal_total_commits().unwrap() - 1;

    assert_eq!(db.edges_at("a", linked).unwrap().len(), 1);
    assert!(db.edges_at("a", gone).unwrap().is_empty());
    // The surviving partner loses the edge as well.
    assert!(db.edges_at("b", gone).unwrap().is_empty());
}

#[test]
fn edges_at_commit_out_of_range_errors() {
    let dir = tmp("range");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "a", vec![]).unwrap();
    let total = db.wal_total_commits().unwrap();

    match db.edges_at("a", total) {
        Err(GraphError::CommitOutOfRange { commit, total: t }) => {
            assert_eq!(commit, total);
            assert_eq!(t, total);
        }
        other => panic!("expected CommitOutOfRange, got {other:?}"),
    }
    match db.edges_at("a", 9_999) {
        Err(GraphError::CommitOutOfRange { .. }) => {}
        other => panic!("expected CommitOutOfRange, got {other:?}"),
    }
}

#[test]
fn edges_at_unknown_key_is_empty_not_an_error() {
    let dir = tmp("unknown");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "a", vec![]).unwrap();
    let last = db.wal_total_commits().unwrap() - 1;
    assert!(db.edges_at("nope", last).unwrap().is_empty());
}

/// Timing probe on a real store. Ignored by default; point
/// `MUSHROOMDB_BENCH_STORE` at a store directory and run with `--ignored`.
#[test]
#[ignore]
fn edges_at_bench_on_large_store() {
    let path = match std::env::var("MUSHROOMDB_BENCH_STORE") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => {
            eprintln!("MUSHROOMDB_BENCH_STORE unset — skipping");
            return;
        }
    };
    let key = std::env::var("MUSHROOMDB_BENCH_KEY").ok();

    let t0 = std::time::Instant::now();
    let db = GraphDb::open_with_options(
        &path,
        OpenOptions {
            auto_migrate: false,
            repair_wal: false,
            read_only: true,
        },
    )
    .unwrap();
    eprintln!("open (read-only): {:?}", t0.elapsed());

    let total = db.wal_total_commits().unwrap();
    let floor = db.wal_horizon_floor();
    eprintln!("wal commits: {floor}..{total}");
    assert!(total > floor, "store has no visible WAL history");

    let key = match key {
        Some(k) => k,
        None => {
            let rs = db
                .query(
                    "MATCH (n) RETURN n LIMIT 1",
                    &std::collections::BTreeMap::new(),
                )
                .expect("sample query");
            assert!(
                !rs.is_empty(),
                "store has no nodes; set MUSHROOMDB_BENCH_KEY"
            );
            match rs.row(0).first().and_then(|c| c.as_ref()) {
                Some(Value::Str(s)) => s.clone(),
                other => panic!("unexpected node cell {other:?}; set MUSHROOMDB_BENCH_KEY"),
            }
        }
    };

    let at = total - 1;
    let t1 = std::time::Instant::now();
    let edges = db.edges_at(&key, at).unwrap();
    let dt = t1.elapsed();
    eprintln!("edges_at({key}, {at}) -> {} edges in {dt:?}", edges.len());
    assert!(
        dt.as_secs_f64() <= 3.0,
        "edges_at took {dt:?} (target <= 3s)"
    );
}

/// Timing probe for `what_if_set_prop` on a real store. Same env contract as
/// `edges_at_bench_on_large_store`; no target is asserted, this only reports.
#[test]
#[ignore]
fn what_if_bench_on_large_store() {
    let path = match std::env::var("MUSHROOMDB_BENCH_STORE") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => {
            eprintln!("MUSHROOMDB_BENCH_STORE unset — skipping");
            return;
        }
    };

    let t0 = std::time::Instant::now();
    let db = GraphDb::open_with_options(
        &path,
        OpenOptions {
            auto_migrate: false,
            repair_wal: false,
            read_only: true,
        },
    )
    .unwrap();
    eprintln!("open (read-only): {:?}", t0.elapsed());

    let rules = db.rules();
    assert!(!rules.is_empty(), "store has no rules; nothing to what-if");
    let rule = &rules[0];
    let field = rule
        .watched_fields()
        .into_iter()
        .next()
        .expect("rule watches no field");

    let cypher = format!("MATCH (n:{}) RETURN n LIMIT 1", rule.src_label);
    let rs = db
        .query(&cypher, &std::collections::BTreeMap::new())
        .expect("sample query");
    assert!(!rs.is_empty(), "no {} nodes", rule.src_label);
    let key = match rs.row(0).first().and_then(|c| c.as_ref()) {
        Some(Value::Str(s)) => s.clone(),
        other => panic!("unexpected node cell {other:?}"),
    };

    let t1 = std::time::Instant::now();
    let wi = db
        .what_if_set_prop(&key, &field, Value::Str("__what_if_probe__".into()))
        .unwrap();
    eprintln!(
        "what_if_set_prop({key}, {field}) -> {} lost / {} gained in {:?}",
        wi.lost.len(),
        wi.gained.len(),
        t1.elapsed()
    );
}
