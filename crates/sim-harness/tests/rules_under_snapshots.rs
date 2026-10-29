//! H-5 (0.10.x hardening): competing rules give the same graph whether or not
//! the store was snapshotted and reopened.
//!
//! Two stores receive the same random sequence. `plain` never snapshots or
//! reopens. `snap` takes a snapshot, or closes and reopens itself, wherever the
//! sequence says so. Every rule in the pool writes the same edge type, so rules
//! compete for edges, take them from each other, cap them (top-k) and weight
//! them under different property names. After every op both stores must
//! return the same outcome for the op, and the same graph: every edge, which
//! rule owns it, and its weight.
//!
//! This is the class of rows 103, 107–113, 116 and 154: a second rule taking an
//! edge across a snapshot, provenance doubled or lost in a snapshot, the first
//! write after a reopen, and a stale weight after a hand-over. Each was a
//! difference between a store that had been snapshotted and one that had not.
//!
//! `Oracle` is not used: it does not model two rules owning one edge type.
//! The two stores check each other.
//!
//! Any divergence is an engine bug. Do not weaken these assertions.
//!
//! Cases: 64 per run by default; set `H5_CASES` to raise it (the nightly run).

use mushroomdb::{Direction, GraphDb, Predicate, RuleDef, Value};
use proptest::prelude::*;
use sim_harness::SimFs;
use std::collections::BTreeSet;

const KEY_MOD: u8 = 10;
const ETYPE: &str = "R";
const TAGS: [&str; 3] = ["a", "b", "c"];

fn rule(name: &str, predicate: Predicate, weight: Option<&str>, cap: Option<u64>) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "P".into(),
        dst_label: "P".into(),
        predicate,
        edge_type: ETYPE.into(),
        weight_prop: weight.map(String::from),
        max_edges: cap,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

/// Four rules over the one edge type: two capped, three weighted under
/// different property names, one uncapped and unweighted.
fn rule_pool(idx: u8) -> RuleDef {
    match idx % 4 {
        0 => rule(
            "fe_k2",
            Predicate::FieldEqual { field: "f".into() },
            Some("w_fe"),
            Some(2),
        ),
        1 => rule(
            "nw_k1",
            Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 5.0,
            },
            Some("w_nw"),
            Some(1),
        ),
        2 => rule(
            "ov",
            Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            Some("w_ov"),
            None,
        ),
        _ => rule(
            "fe_all",
            Predicate::FieldEqual { field: "f".into() },
            None,
            None,
        ),
    }
}

const WEIGHT_FIELDS: [&str; 3] = ["w_fe", "w_nw", "w_ov"];

#[derive(Debug, Clone)]
enum Op {
    InsertNode(u8),
    SetF(u8, u8),
    SetYear(u8, u8),
    SetTags(u8, u8, u8),
    DeleteNode(u8),
    InsertEdge(u8, u8),
    DeleteEdge(u8, u8),
    CreateRule(u8),
    DeleteRule(u8),
    RebuildRule(u8),
    Snapshot,
    Reopen,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => any::<u8>().prop_map(Op::InsertNode),
        3 => (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetF(k, v)),
        3 => (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetYear(k, v)),
        2 => (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(k, a, b)| Op::SetTags(k, a, b)),
        1 => any::<u8>().prop_map(Op::DeleteNode),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(s, d)| Op::InsertEdge(s, d)),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(s, d)| Op::DeleteEdge(s, d)),
        3 => any::<u8>().prop_map(Op::CreateRule),
        1 => any::<u8>().prop_map(Op::DeleteRule),
        1 => any::<u8>().prop_map(Op::RebuildRule),
        2 => Just(Op::Snapshot),
        2 => Just(Op::Reopen),
    ]
}

fn key_of(n: u8) -> String {
    format!("k{}", n % KEY_MOD)
}

fn tags_of(a: u8, b: u8) -> Value {
    let mut items = vec![
        Value::Str(TAGS[(a as usize) % TAGS.len()].into()),
        Value::Str(TAGS[(b as usize) % TAGS.len()].into()),
    ];
    items.dedup();
    Value::List(items)
}

/// Apply one op and describe its outcome as text, so the two stores' outcomes
/// can be compared too: an op that succeeds on one and fails on the other is a
/// divergence.
fn apply(db: &mut GraphDb<SimFs>, op: &Op) -> String {
    fn show<T: std::fmt::Debug>(r: mushroomdb::Result<T>) -> String {
        match r {
            Ok(v) => format!("ok {v:?}"),
            // The error's variant name only: its message may carry ids that
            // legitimately differ between the two stores.
            Err(e) => {
                let debug = format!("{e:?}");
                let variant = debug.split([' ', '{', '(']).next().unwrap_or("");
                format!("err {variant}")
            }
        }
    }
    match op {
        Op::InsertNode(n) => {
            let props = vec![
                ("f".into(), Value::Str(format!("f{}", n % 3))),
                ("year".into(), Value::Float(f64::from(n % 12))),
                ("tags".into(), tags_of(*n, n.wrapping_add(1))),
            ];
            show(db.insert_node("P", &key_of(*n), props))
        }
        Op::SetF(k, v) => show(db.set_prop(&key_of(*k), "f", Value::Str(format!("f{}", v % 3)))),
        Op::SetYear(k, v) => {
            show(db.set_prop(&key_of(*k), "year", Value::Float(f64::from(v % 12))))
        }
        Op::SetTags(k, a, b) => show(db.set_prop(&key_of(*k), "tags", tags_of(*a, *b))),
        Op::DeleteNode(k) => show(db.delete_node(&key_of(*k)).map(|_| ())),
        Op::InsertEdge(s, d) => show(db.insert_edge(ETYPE, &key_of(*s), &key_of(*d))),
        Op::DeleteEdge(s, d) => show(db.delete_edge(ETYPE, &key_of(*s), &key_of(*d))),
        Op::CreateRule(i) => show(db.create_rule(rule_pool(*i))),
        Op::DeleteRule(i) => show(db.delete_rule(&rule_pool(*i).name)),
        Op::RebuildRule(i) => show(db.rebuild_rule(&rule_pool(*i).name)),
        Op::Snapshot | Op::Reopen => unreachable!("handled by the caller"),
    }
}

/// The whole rule-relevant graph as text: every edge of the shared type with
/// each weight field, and every rule ownership `explain` reports, sorted.
fn digest(db: &GraphDb<SimFs>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for s in 0..KEY_MOD {
        let src = key_of(s);
        for dst in db
            .neighbors(&src, ETYPE, Direction::Out)
            .unwrap_or_default()
        {
            let weights: Vec<String> = WEIGHT_FIELDS
                .iter()
                .map(|w| format!("{w}={:?}", db.get_edge_prop(ETYPE, &src, &dst, w)))
                .collect();
            out.insert(format!("edge {src}->{dst} {}", weights.join(" ")));
        }
        for d in s..KEY_MOD {
            if let Ok(found) = db.explain(&src, &key_of(d)) {
                for e in found {
                    out.insert(format!(
                        "owner {} {}->{} by {} weight {:?}",
                        e.edge_type, e.src_key, e.dst_key, e.rule, e.weight
                    ));
                }
            }
        }
    }
    let mut rules: Vec<String> = db.rules().into_iter().map(|r| r.name).collect();
    rules.sort();
    out.insert(format!("rules {rules:?}"));
    out
}

fn reopen(db: GraphDb<SimFs>) -> GraphDb<SimFs> {
    GraphDb::open_with(db.into_fs().surviving_state()).expect("reopen")
}

fn cases() -> u32 {
    std::env::var("H5_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]
    #[test]
    fn competing_rules_agree_across_snapshots_and_reopens(
        ops in proptest::collection::vec(op_strategy(), 1..60),
    ) {
        let mut plain = GraphDb::open_with(SimFs::new()).unwrap();
        let mut snap = GraphDb::open_with(SimFs::new()).unwrap();
        for (step, op) in ops.iter().enumerate() {
            match op {
                Op::Snapshot => snap.snapshot().expect("snapshot"),
                Op::Reopen => snap = reopen(snap),
                _ => {
                    let a = apply(&mut plain, op);
                    let b = apply(&mut snap, op);
                    prop_assert_eq!(&a, &b, "step {} {:?}: outcomes differ", step, op);
                }
            }
            let want = digest(&plain);
            let have = digest(&snap);
            if want != have {
                let missing: Vec<_> = want.difference(&have).collect();
                let extra: Vec<_> = have.difference(&want).collect();
                prop_assert!(
                    false,
                    "step {step} {op:?}: snapshotted store differs\n  only in never-snapshotted: {missing:?}\n  only in snapshotted: {extra:?}"
                );
            }
        }
    }
}

/// The digest must see what it compares: on a small fixed graph with every
/// rule in the pool, edges owned by more than one rule show, with their weights. An edge two
/// rules both derive has one owner, the first rule name in byte order (row 107),
/// so not every rule in the pool owns an edge here.
#[test]
fn the_digest_sees_competing_rules() {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();
    for i in 0..4 {
        db.create_rule(rule_pool(i)).unwrap();
    }
    for n in 0..KEY_MOD {
        apply(&mut db, &Op::InsertNode(n));
    }
    let d = digest(&db);
    let owners: BTreeSet<&str> = d
        .iter()
        .filter_map(|l| {
            l.strip_prefix("owner ")
                .and_then(|l| l.split(" by ").nth(1))
        })
        .filter_map(|l| l.split(' ').next())
        .collect();
    assert!(
        owners.len() >= 2,
        "too few rules own edges: {owners:?}\n{d:#?}"
    );
    assert!(
        d.iter()
            .any(|l| l.starts_with("edge ") && l.contains("w_nw=Some")),
        "no weight seen"
    );
}
