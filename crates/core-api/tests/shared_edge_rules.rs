//! Two rules over one edge.
//!
//! Two rules may share a source label, a destination label and an edge type.
//! When both derive the same directed edge it has one owner, and until 0.7.2
//! the edge was lost when the owner stopped while the other rule still wanted
//! it (ledger row 37). These tests hold the engine to the simplest statement
//! of what is right: after any sequence of writes, the derived edges are the
//! ones a fresh store built from the same nodes would derive.
//!
//! The differential tests and most of the cases here were written by the
//! review of the fix, as experiments, and kept.
use core_api::{Direction, FsyncPolicy, GraphDb, Predicate, RuleDef, Value};
use core_storage::fs::RealFs;
use std::collections::{BTreeMap, BTreeSet};

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("shared-edge-{name}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn tags(xs: &[&str]) -> Value {
    Value::List(xs.iter().map(|s| Value::Str((*s).into())).collect())
}

fn rule(name: &str, src: &str, dst: &str, predicate: Predicate, max_edges: Option<u64>) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: src.into(),
        dst_label: dst.into(),
        predicate,
        edge_type: "SAME".into(),
        weight_prop: Some("weight".into()),
        max_edges,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

fn overlap(min: f64) -> Predicate {
    Predicate::Overlap {
        field: "tags".into(),
        min,
    }
}

fn claim() -> Predicate {
    Predicate::KeyMatch { field: "fk".into() }
}

fn dump(db: &Db, keys: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for s in keys {
        let Ok(ns) = db.neighbors(s, "SAME", Direction::Out) else {
            continue;
        };
        for d in ns {
            let ex = db.explain(s, &d).unwrap();
            let owners: Vec<String> = ex
                .iter()
                .filter(|e| e.src_key == *s && e.dst_key == d)
                .map(|e| format!("{}@{:?}", e.rule, e.weight))
                .collect();
            let w = db.get_edge_prop("SAME", s, &d, "weight");
            out.push(format!("{s}->{d} owners={owners:?} weight={w:?}"));
        }
    }
    out
}

fn stats_line(db: &Db) -> String {
    let st = db.stats();
    let rules: Vec<String> = st
        .rules
        .iter()
        .map(|r| format!("{}:edges={},tripped={}", r.name, r.edges, r.tripped))
        .collect();
    format!("edges={} rules={rules:?}", st.edges)
}

// ---------------------------------------------------------------------------
// The takeover, end to end
// ---------------------------------------------------------------------------

/// The owner stops, the other rule takes the edge at its own score, and the
/// store says the same thing after a WAL-only reopen and after a snapshot.
#[test]
fn the_takeover_survives_a_wal_reopen_and_a_snapshot_reopen() {
    let dir = tmp("takeover-reopen");
    let keys = ["a", "b"];
    let (live, live_stats) = {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node(
            "A",
            "a",
            vec![
                ("tags".into(), tags(&["x", "y", "z"])),
                ("fk".into(), Value::Str("b".into())),
            ],
        )
        .unwrap();
        db.insert_node("A", "b", vec![("tags".into(), tags(&["x", "y"]))])
            .unwrap();
        db.create_rule(rule("claim", "A", "A", claim(), None))
            .unwrap();
        db.create_rule(rule("overlap", "A", "A", overlap(0.6), None))
            .unwrap();
        let before = dump(&db, &keys);
        assert!(
            before
                .iter()
                .any(|l| l.starts_with("a->b") && l.contains("claim@")),
            "the claim rule owns a->b to begin with: {before:?}"
        );

        // `a` stops naming `b`. The overlap, 2 of 3, still holds.
        db.remove_prop("a", "fk").unwrap();
        let after = dump(&db, &keys);
        assert!(
            after
                .iter()
                .any(|l| l.starts_with("a->b") && l.contains("overlap@") && !l.contains("claim@")),
            "the overlap rule took a->b: {after:?}"
        );
        let weight = match db.get_edge_prop("SAME", "a", "b", "weight") {
            Some(Value::Float(w)) => w,
            other => panic!("a->b should carry the overlap's score, got {other:?}"),
        };
        assert!((weight - 2.0 / 3.0).abs() < 1e-9, "got {weight}");
        (after, stats_line(&db))
    };

    // Derived edges are not logged: a reopen derives them again from the WAL.
    {
        let db = GraphDb::open(&dir).unwrap();
        assert_eq!(dump(&db, &keys), live, "after replaying the WAL");
        assert_eq!(stats_line(&db), live_stats);
    }
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.snapshot().unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(dump(&db, &keys), live, "after a snapshot");
    assert_eq!(stats_line(&db), live_stats);
}

/// A top-k rule evicts an edge because a *different* destination became a
/// better match. The edge it drops does not involve the node that changed,
/// and the other rule still wants it.
#[test]
fn an_edge_a_top_k_rule_evicts_is_taken_by_the_rule_that_still_wants_it() {
    let dir = tmp("topk-evicts");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "A",
        "s",
        vec![
            ("tags".into(), tags(&["x", "y"])),
            ("fk".into(), Value::Str("d2".into())),
        ],
    )
    .unwrap();
    db.insert_node("A", "d2", vec![("tags".into(), tags(&["x", "y", "z"]))])
        .unwrap();
    db.insert_node("A", "d1", vec![("tags".into(), tags(&["q"]))])
        .unwrap();
    db.create_rule(rule("atopk", "A", "A", overlap(0.3), Some(1)))
        .unwrap();
    db.create_rule(rule("claim", "A", "A", claim(), None))
        .unwrap();
    db.set_prop("d1", "tags", tags(&["x", "y"])).unwrap();
    let after = dump(&db, &["s", "d1", "d2"]);
    assert!(after.iter().any(|l| l.starts_with("s->d2")), "{after:?}");
    assert!(after.iter().any(|l| l.starts_with("s->d1")), "{after:?}");
}

/// P -[WORKS_AT]-> O, and a via-hop rule links P to T through it. A plain rule
/// over the same labels and edge type wants p->t as well.
fn via_and_claim(name: &str) -> (std::path::PathBuf, Db) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "p", vec![("fk".into(), Value::Str("t".into()))])
        .unwrap();
    db.insert_node("O", "o", vec![("tags".into(), tags(&["x", "y"]))])
        .unwrap();
    db.insert_node("T", "t", vec![("tags".into(), tags(&["x", "y"]))])
        .unwrap();
    db.insert_edge("WORKS_AT", "p", "o").unwrap();
    let mut via = rule("avia", "P", "T", overlap(0.6), None);
    via.via_label = Some("O".into());
    via.via_edge = Some("WORKS_AT".into());
    via.via_dir = Some(Direction::Out);
    db.create_rule(via).unwrap();
    db.create_rule(rule("claim", "P", "T", claim(), None))
        .unwrap();
    (dir, db)
}

/// The usual way a via-hop rule stops: its hop edge is deleted.
#[test]
fn a_via_rules_edge_is_taken_by_a_plain_rule_when_the_hop_is_deleted() {
    let (_dir, mut db) = via_and_claim("via-hop-deleted");
    db.delete_edge("WORKS_AT", "p", "o").unwrap();
    let after = dump(&db, &["p", "o", "t"]);
    assert!(
        after
            .iter()
            .any(|l| l.starts_with("p->t") && l.contains("claim@")),
        "the claim rule still wants p->t and takes it: {after:?}"
    );
}

/// And the other way it stops: the node it hops through changes.
#[test]
fn a_via_rules_edge_is_taken_by_a_plain_rule_when_the_hop_node_changes() {
    let (_dir, mut db) = via_and_claim("via-hop-node-changed");
    db.set_prop("o", "tags", tags(&["q"])).unwrap();
    let after = dump(&db, &["p", "o", "t"]);
    assert!(
        after
            .iter()
            .any(|l| l.starts_with("p->t") && l.contains("claim@")),
        "{after:?}"
    );
}

/// The reverse: a plain rule owns the edge, a via-hop rule wants it too, and
/// the plain rule stops. The second pass skipped via-hop rules (ledger row
/// 84); it now re-evaluates them along their hop.
#[test]
fn a_via_rule_takes_an_edge_a_plain_rule_retracted() {
    let dir = tmp("plain-stops-via-wants");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "p", vec![("fk".into(), Value::Str("t".into()))])
        .unwrap();
    db.insert_node("O", "o", vec![("tags".into(), tags(&["x", "y"]))])
        .unwrap();
    db.insert_node("T", "t", vec![("tags".into(), tags(&["x", "y"]))])
        .unwrap();
    db.insert_edge("WORKS_AT", "p", "o").unwrap();
    db.create_rule(rule("aclaim", "P", "T", claim(), None))
        .unwrap();
    let mut via = rule("via", "P", "T", overlap(0.6), None);
    via.via_label = Some("O".into());
    via.via_edge = Some("WORKS_AT".into());
    via.via_dir = Some(Direction::Out);
    db.create_rule(via).unwrap();
    db.remove_prop("p", "fk").unwrap();
    let after = dump(&db, &["p", "o", "t"]);
    assert!(
        after.iter().any(|l| l.starts_with("p->t")),
        "the via rule still wants p->t: {after:?}"
    );
}

/// The second pass, re-evaluating a via-hop rule that links a label to
/// itself, must keep the edges *into* the source. Its first form computed the
/// source's outgoing desired set only, against a retract set of every edge of
/// the rule touching the source, so `x->s` went when `s` was reclaimed — an
/// edge `main` had right. Found by the review of row 84's fix.
#[test]
fn a_self_label_via_rule_reclaimed_for_a_source_keeps_the_edges_into_it() {
    let build = |name: &str, with_fk: bool| -> (std::path::PathBuf, Db) {
        let dir = tmp(name);
        let mut db = GraphDb::open(&dir).unwrap();
        let mut s_props = vec![("tags".into(), tags(&["x", "y"]))];
        if with_fk {
            s_props.push(("fk".into(), Value::Str("t".into())));
        }
        db.insert_node("P", "s", s_props).unwrap();
        for k in ["t", "x"] {
            db.insert_node("P", k, vec![("tags".into(), tags(&["x", "y"]))])
                .unwrap();
        }
        db.insert_node("O", "o", vec![("tags".into(), tags(&["x", "y"]))])
            .unwrap();
        db.insert_edge("WORKS_AT", "s", "o").unwrap();
        db.insert_edge("WORKS_AT", "x", "o").unwrap();
        db.create_rule(rule("aclaim", "P", "P", claim(), None))
            .unwrap();
        let mut via = rule("via", "P", "P", overlap(0.6), None);
        via.via_label = Some("O".into());
        via.via_edge = Some("WORKS_AT".into());
        via.via_dir = Some(Direction::Out);
        db.create_rule(via).unwrap();
        (dir, db)
    };
    let edges = |db: &Db| -> Vec<String> {
        let mut out = Vec::new();
        for k in ["s", "t", "x"] {
            for d in db.neighbors(k, "SAME", Direction::Out).unwrap() {
                out.push(format!("{k}->{d}"));
            }
        }
        out.sort();
        out
    };
    let (_d1, mut live) = build("self-via-live", true);
    live.remove_prop("s", "fk").unwrap();
    let (_d2, fresh) = build("self-via-fresh", false);
    assert_eq!(edges(&live), edges(&fresh));
    assert!(edges(&live).contains(&"x->s".to_string()));
}

// ---------------------------------------------------------------------------
// Against a fresh build
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct N {
    label: String,
    tags: Option<Vec<String>>,
    fk: Option<String>,
}

fn props_of(n: &N) -> Vec<(String, Value)> {
    let mut p = Vec::new();
    if let Some(t) = &n.tags {
        p.push((
            "tags".to_string(),
            Value::List(t.iter().map(|s| Value::Str(s.clone())).collect()),
        ));
    }
    if let Some(f) = &n.fk {
        p.push(("fk".to_string(), Value::Str(f.clone())));
    }
    p
}

fn edge_set(db: &Db, model: &BTreeMap<String, N>) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for k in model.keys() {
        for d in db.neighbors(k, "SAME", Direction::Out).unwrap() {
            out.insert((k.clone(), d));
        }
    }
    out
}

fn owner_dump(db: &Db, model: &BTreeMap<String, N>) -> Vec<String> {
    let keys: Vec<&str> = model.keys().map(|s| s.as_str()).collect();
    dump(db, &keys)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn rand_tags(r: &mut Rng) -> Vec<String> {
    let all = ["x", "y", "z", "w"];
    let mut v: Vec<String> = all
        .iter()
        .filter(|_| r.below(2) == 0)
        .map(|s| s.to_string())
        .collect();
    if v.is_empty() {
        v.push(all[r.below(4) as usize].to_string());
    }
    v
}

fn fuzz(
    name: &str,
    rules: &[RuleDef],
    labels: &[&str],
    seeds: std::ops::Range<u64>,
    steps: usize,
) -> (usize, usize) {
    let mut set_div = 0usize;
    let mut replay_div = 0usize;
    let mut first: Option<String> = None;
    for seed in seeds {
        let mut r = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let dir = tmp(&format!("fuzz-{name}-{seed}"));
        let mut db = GraphDb::open(&dir).unwrap();
        db.set_fsync_policy(FsyncPolicy::Relaxed);
        let mut model: BTreeMap<String, N> = BTreeMap::new();
        for rd in rules {
            db.create_rule(rd.clone()).unwrap();
        }
        let mut next_id = 0u32;
        let mut log: Vec<String> = Vec::new();
        let mut diverged = false;
        for _ in 0..steps {
            let keys: Vec<String> = model.keys().cloned().collect();
            let op = r.below(10);
            if keys.len() < 3 || op == 0 {
                let k = format!("n{next_id}");
                next_id += 1;
                let all_keys_n = keys.len() as u64 + 1;
                let fk = if r.below(2) == 0 && !keys.is_empty() {
                    Some(keys[r.below(all_keys_n - 1) as usize].clone())
                } else {
                    None
                };
                let n = N {
                    label: labels[r.below(labels.len() as u64) as usize].to_string(),
                    tags: Some(rand_tags(&mut r)),
                    fk,
                };
                db.insert_node(&n.label, &k, props_of(&n)).unwrap();
                log.push(format!("insert {k} {n:?}"));
                model.insert(k, n);
            } else {
                let k = keys[r.below(keys.len() as u64) as usize].clone();
                match op {
                    1..=4 => {
                        let t = rand_tags(&mut r);
                        db.set_prop(
                            &k,
                            "tags",
                            Value::List(t.iter().map(|s| Value::Str(s.clone())).collect()),
                        )
                        .unwrap();
                        log.push(format!("set {k}.tags={t:?}"));
                        model.get_mut(&k).unwrap().tags = Some(t);
                    }
                    5..=6 => {
                        let f = keys[r.below(keys.len() as u64) as usize].clone();
                        db.set_prop(&k, "fk", Value::Str(f.clone())).unwrap();
                        log.push(format!("set {k}.fk={f}"));
                        model.get_mut(&k).unwrap().fk = Some(f);
                    }
                    7 => {
                        db.remove_prop(&k, "fk").unwrap();
                        log.push(format!("remove {k}.fk"));
                        model.get_mut(&k).unwrap().fk = None;
                    }
                    8 => {
                        db.remove_prop(&k, "tags").unwrap();
                        log.push(format!("remove {k}.tags"));
                        model.get_mut(&k).unwrap().tags = None;
                    }
                    _ => {
                        db.delete_node(&k).unwrap();
                        log.push(format!("delete {k}"));
                        model.remove(&k);
                    }
                }
            }
            // Oracle: a fresh store holding the same nodes, rules created last.
            let odir = tmp(&format!("oracle-{name}-{seed}"));
            let mut o = GraphDb::open(&odir).unwrap();
            o.set_fsync_policy(FsyncPolicy::Relaxed);
            for (k, n) in &model {
                o.insert_node(&n.label, k, props_of(n)).unwrap();
            }
            for rd in rules {
                o.create_rule(rd.clone()).unwrap();
            }
            let live = edge_set(&db, &model);
            let want = edge_set(&o, &model);
            drop(o);
            let _ = std::fs::remove_dir_all(&odir);
            if live != want && !diverged {
                diverged = true;
                set_div += 1;
                if first.is_none() {
                    let missing: Vec<_> = want.difference(&live).collect();
                    let extra: Vec<_> = live.difference(&want).collect();
                    first = Some(format!(
                        "seed {seed} after {} ops: missing={missing:?} extra={extra:?}\n  model={model:?}\n  owners={:?}\n  ops={:#?}",
                        log.len(),
                        owner_dump(&db, &model),
                        &log
                    ));
                }
            }
        }
        // Replay determinism: live state vs WAL-only reopen, owners and weights.
        let live = owner_dump(&db, &model);
        let live_stats = stats_line(&db);
        drop(db);
        let db2 = GraphDb::open(&dir).unwrap();
        if owner_dump(&db2, &model) != live || stats_line(&db2) != live_stats {
            replay_div += 1;
            println!(
                "REPLAY DIVERGENCE {name} seed {seed}:\n live={live:?}\n replay={:?}",
                owner_dump(&db2, &model)
            );
        }
        drop(db2);
        let _ = std::fs::remove_dir_all(&dir);
    }
    if let Some(f) = first {
        println!("FUZZ {name}: first divergence: {f}");
    }
    (set_div, replay_div)
}

/// `(seeds whose live edges ever differed from a fresh build, seeds whose
/// WAL-only reopen differed from the live store)`. Both are wanted at zero.
const AGREES: (usize, usize) = (0, 0);

#[test]
fn two_plain_rules_over_one_label_always_match_a_fresh_build() {
    let rules = vec![
        rule("claim", "A", "A", claim(), None),
        rule("overlap", "A", "A", overlap(0.6), None),
    ];
    assert_eq!(fuzz("plain-self", &rules, &["A"], 0..20, 40), AGREES);
}

#[test]
fn two_plain_rules_between_two_labels_always_match_a_fresh_build() {
    let rules = vec![
        rule("claim", "A", "B", claim(), None),
        rule("overlap", "A", "B", overlap(0.6), None),
    ];
    assert_eq!(fuzz("plain-ab", &rules, &["A", "B"], 0..20, 40), AGREES);
}

/// The control: one rule has nothing to collide with, and never diverged.
#[test]
fn one_rule_alone_always_matches_a_fresh_build() {
    let plain = vec![rule("overlap", "A", "A", overlap(0.6), None)];
    assert_eq!(fuzz("single", &plain, &["A"], 0..10, 40), AGREES);
    let top_k = vec![rule("topk", "A", "A", overlap(0.3), Some(1))];
    assert_eq!(fuzz("single-topk", &top_k, &["A"], 0..10, 40), AGREES);
}

/// Ledger row 83. A top-k rule whose best match is owned by another rule
/// records nothing for that source, and gives up its own next-best edge. When
/// that match went away — a changed tag, or the destination deleted — the
/// source was found through the rule's provenance alone, was not recomputed,
/// and the next-best edge stayed missing until a rebuild. Nothing is retracted
/// on the shared edge, so the second pass has nothing to see; the sources are
/// now found through the edges into the destination as well. A via-hop top-k
/// rule still has the defect; this test has none.
#[test]
fn a_top_k_rule_beside_a_sibling_always_matches_a_fresh_build() {
    let rules = vec![
        rule("claim", "A", "A", claim(), None),
        rule("overlap", "A", "A", overlap(0.6), None),
        rule("topk", "A", "A", overlap(0.3), Some(1)),
    ];
    assert_eq!(fuzz("with-topk", &rules, &["A"], 0..20, 40), AGREES);
}

/// Ledger row 106. Deleting the node a KeyMatch top-k rule's source names
/// re-picked that node: the backfill that finds the source's next-best match
/// read its candidates by key, and the node being deleted still answers to
/// its key at that point. The edge went back in with its provenance, the
/// delete then swept it from the graph, and `stats` counted an edge
/// `neighbors` did not return — where the next-best was there to be taken.
#[test]
fn deleting_a_top_k_rules_best_match_promotes_the_next_one() {
    for k in [1u64, 2] {
        let dir = tmp(&format!("topk-delete-{k}"));
        let mut db = GraphDb::open(&dir).unwrap();
        db.create_rule(rule(
            "tk",
            "A",
            "A",
            Predicate::KeyMatch {
                field: "fkl".into(),
            },
            Some(k),
        ))
        .unwrap();
        for key in ["n0", "n7"] {
            db.insert_node("A", key, vec![]).unwrap();
        }
        let fks = Value::List(vec![Value::Str("n7".into()), Value::Str("n0".into())]);
        db.insert_node("A", "s", vec![("fkl".into(), fks)]).unwrap();
        db.delete_node("n0").unwrap();

        assert_eq!(
            db.neighbors("s", "SAME", Direction::Out).unwrap(),
            vec!["n7".to_string()],
            "k={k}"
        );
        let derived: u64 = db.stats().rules.iter().map(|r| r.edges).sum();
        assert_eq!(derived, 1, "k={k}: the count agrees with the graph");
    }
}

/// Ledger row 105. A via-hop rule that links a label to itself, inserting a
/// second node that hops nowhere: the edge into the new node from a source
/// that reaches it was derived and then retracted again when the new node
/// was evaluated as a source, because that evaluation's desired set held its
/// outgoing edges only. Whether the edge survived depended on the order the
/// nodes were evaluated in.
#[test]
fn a_self_label_via_rule_derives_the_edge_into_a_new_node() {
    let dir = tmp("self-via-insert");
    let mut db = GraphDb::open(&dir).unwrap();
    let mut via = rule("via", "P", "P", overlap(0.6), None);
    via.via_label = Some("O".into());
    via.via_edge = Some("WORKS_AT".into());
    via.via_dir = Some(Direction::Out);
    db.create_rule(via).unwrap();
    db.insert_node("P", "n0", vec![("tags".into(), tags(&["y", "w"]))])
        .unwrap();
    db.insert_node("O", "n2", vec![("tags".into(), tags(&["y", "z", "w"]))])
        .unwrap();
    db.insert_edge("WORKS_AT", "n0", "n2").unwrap();
    db.insert_node("P", "n5", vec![("tags".into(), tags(&["y", "z"]))])
        .unwrap();
    let live = db.neighbors("n0", "SAME", Direction::Out).unwrap();
    db.rebuild_rule("via").unwrap();
    let rebuilt = db.neighbors("n0", "SAME", Direction::Out).unwrap();
    assert_eq!(live, rebuilt);
    assert!(live.contains(&"n5".to_string()), "{live:?}");
}

/// Ledger row 104. Deleting a via-hop rule's via node left the edge the rule
/// had derived through it: the delete retracts edges *touching* the deleted
/// node, and the via node is on neither end of the edge it carried. The
/// sources that hopped through it are now evaluated again without it.
#[test]
fn deleting_a_via_node_retracts_the_edges_derived_through_it() {
    for k in [None, Some(1u64)] {
        let dir = tmp(&format!("via-node-delete-{k:?}"));
        let mut db = GraphDb::open(&dir).unwrap();
        let mut via = rule("via", "P", "T", overlap(0.3), k);
        via.via_label = Some("O".into());
        via.via_edge = Some("WORKS_AT".into());
        via.via_dir = Some(Direction::Out);
        db.create_rule(via).unwrap();
        db.insert_node("O", "o", vec![("tags".into(), tags(&["x", "y", "w"]))])
            .unwrap();
        db.insert_node("P", "p", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
        db.insert_node("T", "t", vec![("tags".into(), tags(&["x", "y", "z"]))])
            .unwrap();
        db.insert_edge("WORKS_AT", "p", "o").unwrap();
        assert_eq!(
            db.neighbors("p", "SAME", Direction::Out).unwrap(),
            vec!["t".to_string()],
            "k={k:?}: the fixture derives p->t through o"
        );
        db.delete_node("o").unwrap();
        assert_eq!(
            db.neighbors("p", "SAME", Direction::Out).unwrap(),
            Vec::<String>::new(),
            "k={k:?}"
        );
    }
}

/// Ledger row 103. Two top-k rules over one edge type, a snapshot and a
/// reopen around a delete: the rules' derived edges were counted and not
/// returned. A fresh build of the same nodes has `n0->n5` and `n5->n0`.
#[test]
fn top_k_rules_edges_are_readable_after_a_snapshot_round_trip() {
    let rules = [
        rule("topk1", "A", "A", overlap(0.3), Some(1)),
        rule("topk2", "A", "A", overlap(0.5), Some(2)),
    ];
    let dir = tmp("row103");
    let snapshot_and_reopen = |db: Db| -> Db {
        let mut db = db;
        db.snapshot().unwrap();
        drop(db);
        GraphDb::open(&dir).unwrap()
    };
    let mut db = GraphDb::open(&dir).unwrap();
    for rd in &rules {
        db.create_rule(rd.clone()).unwrap();
    }
    db.insert_node("A", "n0", vec![("tags".into(), tags(&["x", "y", "w"]))])
        .unwrap();
    db.insert_node("A", "n5", vec![("tags".into(), tags(&["x", "y", "z"]))])
        .unwrap();
    db.insert_node(
        "A",
        "n6",
        vec![
            ("tags".into(), tags(&["x", "y", "z"])),
            ("fk".into(), Value::Str("n5".into())),
        ],
    )
    .unwrap();
    let mut db = snapshot_and_reopen(db);
    db.delete_node("n6").unwrap();
    let mut db = snapshot_and_reopen(db);
    db.set_prop("n5", "tags", tags(&["x"])).unwrap();

    let odir = tmp("row103-oracle");
    let mut o = GraphDb::open(&odir).unwrap();
    o.insert_node("A", "n0", vec![("tags".into(), tags(&["x", "y", "w"]))])
        .unwrap();
    o.insert_node("A", "n5", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    for rd in &rules {
        o.create_rule(rd.clone()).unwrap();
    }
    for k in ["n0", "n5"] {
        assert_eq!(
            db.neighbors(k, "SAME", Direction::Out).unwrap(),
            o.neighbors(k, "SAME", Direction::Out).unwrap(),
            "{k}"
        );
    }
    let counted: u64 = db.stats().rules.iter().map(|r| r.edges).sum();
    let present = ["n0", "n5"]
        .iter()
        .map(|k| db.neighbors(k, "SAME", Direction::Out).unwrap().len() as u64)
        .sum::<u64>();
    assert_eq!(counted, present, "every edge a rule counts is in the graph");
}

/// Ledger row 107. A store snapshotted while two rules owned one edge keeps
/// both owners and a wrong `edge_count` header.  The review table (reproduced
/// as assertions below) shows what breaks on the damaged store; the fix is:
///
/// (a) deduplicate provenance at the first write – keeping the first owner in
///     BTree (byte order on rule name) order – so every triple has exactly one
///     owner before any subsequent write happens, and
/// (b) recount `edge_count` in the snapshot encoder by summing the merged
///     CSR rows directly rather than carrying the header value forward
///     arithmetically.
///
/// The `inject_double_ownership_for_test` hook writes a second rule's claim
/// onto an edge that is already in the base CSR, then takes a snapshot that
/// double-counts that edge in the header – reproducing the state that row 103
/// created on stores written before 0.8.1.
///
/// Requires `--features test-hooks` (same gate as the hook on `GraphDb`).
#[cfg(feature = "test-hooks")]
#[test]
fn damaged_store_opened_by_fixed_code_deduplicates_and_recounts() {
    // Two overlap rules that both fire on n1→n2 (Jaccard = 0.5 ≥ both
    // thresholds).  "owner1" sorts before "owner2", so the dedup pass keeps
    // "owner1" as the sole owner on a damaged store.
    let rules = [
        rule("owner1", "A", "A", overlap(0.3), None),
        rule("owner2", "A", "A", overlap(0.5), None),
    ];
    let dir = tmp("row107");

    // ── Step 1: build a clean store; both rules derive n1→n2 and n2→n1 ──────
    let mut db = GraphDb::open(&dir).unwrap();
    for rd in &rules {
        db.create_rule(rd.clone()).unwrap();
    }
    // tags: Jaccard(n1, n2) = |{x,y}| / |{x,y,z,w}| = 2/4 = 0.5 ≥ both thresholds
    db.insert_node("A", "n1", vec![("tags".into(), tags(&["x", "y", "z"]))])
        .unwrap();
    db.insert_node("A", "n2", vec![("tags".into(), tags(&["x", "y", "w"]))])
        .unwrap();

    // Both directed edges are derived; each is owned by exactly one rule.
    let out_n1_clean = db.neighbors("n1", "SAME", Direction::Out).unwrap();
    let out_n2_clean = db.neighbors("n2", "SAME", Direction::Out).unwrap();
    assert_eq!(out_n1_clean, vec!["n2"], "clean store: n1→n2 derived");
    assert_eq!(out_n2_clean, vec!["n1"], "clean store: n2→n1 derived");

    // ── Step 2: snapshot to move the edges into the base CSR ─────────────────
    db.snapshot().unwrap();
    drop(db);
    let mut db = GraphDb::open(&dir).unwrap();

    // ── Step 3: inject double ownership on n1→n2 only ────────────────────────
    // This replicates the pre-0.8.1 condition: "owner2" claims an edge that
    // "owner1" already owns in the base.  The overlay now has a second copy of
    // n1→n2, so the next snapshot's header will read edge_count = 3 (one too
    // many: n1→n2 counted once from base + once from overlay, plus n2→n1).
    db.inject_double_ownership_for_test("owner2", "SAME", "n1", "n2");

    // ── Step 4: snapshot while double ownership is live ───────────────────────
    // The encoder's pre-fix arithmetic would carry this inflated count forward.
    // The fix (recount from merged rows) produces the correct count of 2.
    db.snapshot().unwrap();
    drop(db);

    // ── Step 5: open the damaged store ───────────────────────────────────────
    // With the fix: dedup at the first write removes "owner2"'s duplicate
    // claim; the corrected_edge_count() path gives the right answer at open.
    let mut db = GraphDb::open(&dir).unwrap();

    // (a) Both edges are still readable – the topology itself is correct.
    assert_eq!(
        db.neighbors("n1", "SAME", Direction::Out).unwrap(),
        vec!["n2"],
        "damaged store (fixed): n1→n2 is readable after open"
    );
    assert_eq!(
        db.neighbors("n2", "SAME", Direction::Out).unwrap(),
        vec!["n1"],
        "damaged store (fixed): n2→n1 is readable after open"
    );

    // (b) stats().edges must equal the actual neighbor count – not the inflated
    //     header value.  This covers the review table row:
    //     "snapshotted, open → edge_count wrong (pre-fix) → correct (post-fix)".
    let stats_at_open = db.stats();
    let edge_count_open = stats_at_open.edges;
    let actual_open: u64 = ["n1", "n2"]
        .iter()
        .map(|k| db.neighbors(k, "SAME", Direction::Out).unwrap().len() as u64)
        .sum();
    assert_eq!(
        edge_count_open, actual_open,
        "damaged store (fixed): stats().edges equals actual neighbor count at open"
    );

    // (c) Per-rule edge counts at open.  The dedup pass runs lazily at the
    //     first *write*, not at open, so stats().rules still reflects the
    //     duplicated state here: owner1 sees 2, owner2 still sees its injected
    //     duplicate claim of 1.  This is intentional and documented behaviour.
    let owner1_at_open = stats_at_open
        .rules
        .iter()
        .find(|r| r.name == "owner1")
        .expect("owner1 present");
    let owner2_at_open = stats_at_open
        .rules
        .iter()
        .find(|r| r.name == "owner2")
        .expect("owner2 present");
    assert_eq!(
        owner1_at_open.edges, 2,
        "at open: owner1 reports 2 edges (n1→n2 and n2→n1)"
    );
    assert_eq!(
        owner2_at_open.edges, 1,
        "at open: owner2 still reports its duplicate claim (dedup deferred to first write)"
    );

    // ── Step 6: the write that used to lose n1→n2 ────────────────────────────
    // Change n2's tags so "owner2" no longer qualifies (Jaccard drops to
    // 1/3 < 0.5) but "owner1" still does (1/3 ≥ 0.3).
    //
    // Pre-fix flow with double ownership:
    //   owner2 retracts n1→n2 → g.topo.remove_edge runs unconditionally →
    //   reclaim_retracted(owner1) finds prov.contains(triple)=true → skips →
    //   edge gone from topology forever.
    //
    // Post-fix flow after dedup (runs at the start of this write):
    //   owner2 has no claim on n1→n2; only owner1 does →
    //   owner2's retract loop finds nothing to retract → edge untouched.
    db.set_prop("n2", "tags", tags(&["y"])).unwrap();

    // n1→n2: owner1 still wants it (Jaccard(["x","y","z"], ["y"]) = 1/3 ≥ 0.3)
    assert_eq!(
        db.neighbors("n1", "SAME", Direction::Out).unwrap(),
        vec!["n2"],
        "damaged store (fixed): n1→n2 survives the write that used to lose it"
    );
    // n2→n1: same Jaccard from n2's side (1/3 ≥ 0.3) → owner1 still wants it.
    // Only the n1→n2 edge was double-owned; n2→n1 was always single-owned.
    assert_eq!(
        db.neighbors("n2", "SAME", Direction::Out).unwrap(),
        vec!["n1"],
        "damaged store (fixed): n2→n1 survives (owner1 threshold 0.3 ≤ 1/3)"
    );

    // stats().edges still agrees with the actual topology after the write.
    let stats_after_write = db.stats();
    let edge_count_after_write = stats_after_write.edges;
    let actual_after_write: u64 = ["n1", "n2"]
        .iter()
        .map(|k| db.neighbors(k, "SAME", Direction::Out).unwrap().len() as u64)
        .sum();
    assert_eq!(
        edge_count_after_write, actual_after_write,
        "damaged store (fixed): stats().edges equals actual count after write"
    );

    // Per-rule counts after the write: dedup has now run (inside
    // ensure_provenance_loaded_mut), so owner2's duplicate claim is gone.
    let owner1_after = stats_after_write
        .rules
        .iter()
        .find(|r| r.name == "owner1")
        .expect("owner1 present");
    let owner2_after = stats_after_write
        .rules
        .iter()
        .find(|r| r.name == "owner2")
        .expect("owner2 present");
    assert_eq!(
        owner1_after.edges, 2,
        "after write: owner1 still owns n1→n2 and n2→n1"
    );
    assert_eq!(
        owner2_after.edges, 0,
        "after write: owner2 has 0 edges (dedup removed its duplicate claim)"
    );

    // ── Step 7: snapshot the repaired state and reopen ───────────────────────
    // The encoder now recounts from the merged CSR rows, so the new snapshot's
    // header is correct regardless of the old header's value.
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();

    assert_eq!(
        db.neighbors("n1", "SAME", Direction::Out).unwrap(),
        vec!["n2"],
        "damaged store (fixed): n1→n2 present after snapshot of repaired state"
    );
    let edge_count_after_snap = db.stats().edges;
    let actual_after_snap: u64 = ["n1", "n2"]
        .iter()
        .map(|k| db.neighbors(k, "SAME", Direction::Out).unwrap().len() as u64)
        .sum();
    assert_eq!(
        edge_count_after_snap, actual_after_snap,
        "damaged store (fixed): stats().edges equals actual count after snapshot round-trip"
    );

    // The encoder fix: the snapshot that was written in Step 4 (damaged state)
    // must have a correct header value.  Without the encoder fix, the arithmetic
    // path (base_count + overlay_count - tombstones) inflates the count by one
    // for every edge the overlay duplicated from the base — giving 3, not 2.
    // This assertion reads the raw header value directly (bypassing the
    // corrected_edge_count row-walk) so it fails if only the encoder is reverted.
    assert_eq!(
        db.snapshot_header_edge_count_for_test(),
        Some(2),
        "encoder fix: snapshot header edge_count must equal the actual merged row count (2)"
    );
}

/// Ledger row 107 — real v0.8.0 fixture.
///
/// This store was written by `z1_prefix(&dir, true)` running against the v0.8.0
/// tag (which had the row-103 bug).  It has two overlap rules — `topk1`
/// (min=0.3, k=1) and `topk2` (min=0.5, k=2) — over nodes `n0` (tags: x,y,w)
/// and `n5` (tags: x,y,z).  Jaccard = 0.5, so both thresholds fire.  The
/// second snapshot was taken after deleting `n6`; at that point the row-103 bug
/// let both rules claim n0→n5 and n5→n0, inflating the snapshot header's
/// `edge_count` to 4 (two actual edges, four ownership records).
///
/// This test is the proof that the `corrected_edge_count` row-walk fix
/// (db.rs) is needed independently of the encoder fix: the encoder fix cannot
/// retroactively correct a snapshot header written by old code, but the row-walk
/// reads the CSR adjacency data directly and always gives the correct answer.
#[test]
fn real_v080_damaged_store_is_repaired_on_open() {
    // Write the fixture files to a fresh tmpdir.
    let dir = tmp("row107-v080");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("snapshot.bin"),
        &include_bytes!("fixtures/v080_snapshotted/snapshot.bin")[..],
    )
    .unwrap();
    std::fs::write(
        dir.join("wal.bin"),
        &include_bytes!("fixtures/v080_snapshotted/wal.bin")[..],
    )
    .unwrap();
    std::fs::write(
        dir.join("wal.floor"),
        &include_bytes!("fixtures/v080_snapshotted/wal.floor")[..],
    )
    .unwrap();

    // ── Open the damaged store with the fixed code ────────────────────────────
    let mut db = GraphDb::open(&dir).unwrap();

    // (a) Both edges are readable — topology itself is correct.
    let n0_out = db.neighbors("n0", "SAME", Direction::Out).unwrap();
    let n5_out = db.neighbors("n5", "SAME", Direction::Out).unwrap();
    assert_eq!(n0_out, vec!["n5"], "v080 fixture: n0→n5 readable at open");
    assert_eq!(n5_out, vec!["n0"], "v080 fixture: n5→n0 readable at open");

    // (b) stats().edges must equal the actual neighbor count.
    //     The snapshot header says edge_count = 4; the corrected_edge_count
    //     row-walk gives 2.  This assertion fails if the db.rs fix is reverted.
    let actual: u64 = ["n0", "n5"]
        .iter()
        .map(|k| db.neighbors(k, "SAME", Direction::Out).unwrap().len() as u64)
        .sum();
    assert_eq!(
        db.stats().edges,
        actual,
        "v080 fixture: stats().edges must equal actual neighbor count (2), not header (4)"
    );
    assert_eq!(actual, 2, "v080 fixture: exactly 2 edges present");

    // (c) The snapshot header itself records the old (wrong) value — 4.
    //     This is the value the db.rs fix must compensate for.
    assert_eq!(
        db.snapshot_header_edge_count_for_test(),
        Some(4),
        "v080 fixture: snapshot header is 4 (inflated by row-107 bug)"
    );

    // ── The write that used to lose n0→n5 ────────────────────────────────────
    // Drop n5's tags to ["x"]: Jaccard(n0, n5) = |{x}|/|{x,y,z,w}| = 1/3.
    // topk1 threshold 0.3: 1/3 ≥ 0.3 → topk1 still wants n0→n5.
    // topk2 threshold 0.5: 1/3 < 0.5 → topk2 stops wanting n0→n5.
    // Without dedup, topk2's retract would wipe n0→n5 before topk1 could
    // re-add it; with dedup, topk2 has no claim and the edge is untouched.
    db.set_prop("n5", "tags", tags(&["x"])).unwrap();

    assert_eq!(
        db.neighbors("n0", "SAME", Direction::Out).unwrap(),
        vec!["n5"],
        "v080 fixture: n0→n5 survives the write that used to lose it"
    );
    let actual_after: u64 = ["n0", "n5"]
        .iter()
        .map(|k| db.neighbors(k, "SAME", Direction::Out).unwrap().len() as u64)
        .sum();
    assert_eq!(
        db.stats().edges,
        actual_after,
        "v080 fixture: stats().edges equals actual count after write"
    );

    // ── Snapshot round-trip ───────────────────────────────────────────────────
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();

    assert_eq!(
        db.neighbors("n0", "SAME", Direction::Out).unwrap(),
        vec!["n5"],
        "v080 fixture: n0→n5 present after snapshot round-trip"
    );
    let actual_snap: u64 = ["n0", "n5"]
        .iter()
        .map(|k| db.neighbors(k, "SAME", Direction::Out).unwrap().len() as u64)
        .sum();
    assert_eq!(
        db.stats().edges,
        actual_snap,
        "v080 fixture: stats().edges equals actual count after snapshot round-trip"
    );
    // The encoder fix: the new snapshot has a correct header.
    assert_eq!(
        db.snapshot_header_edge_count_for_test(),
        Some(actual_snap),
        "v080 fixture: snapshot round-trip writes a correct header"
    );
}
