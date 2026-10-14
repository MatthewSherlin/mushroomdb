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
