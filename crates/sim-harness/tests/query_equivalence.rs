//! Cypher ↔ traversal equivalence over random small graphs.
//!
//! Graphs are built only through the public `GraphDb` API (SimFs). Exactly one
//! scored Overlap rule is created mid-sequence. For a sample of live nodes and
//! every etype in play:
//!   (a) `MATCH (a {k: $key})-[r:T]->(b) RETURN b` row set == `neighbors` Out
//!   (b) 1-hop undirected Cypher row set == `grouped_by_edge_type` bucket
//!
//! Any divergence is an engine bug. Do not weaken these assertions.
//!
//! The proptest may see empty==empty when Overlap does not fire on a given
//! graph. Non-empty derived-edge equality is pinned by
//! `overlap_rule_equality_on_nonempty_derived_sets`.

use core_api::{Direction, GraphDb, Predicate, RuleDef, Value};
use proptest::prelude::*;
use sim_harness::SimFs;
use std::collections::{BTreeMap, BTreeSet};

const TAGS: [&str; 3] = ["a", "b", "c"];
const USER_ETYPES: [&str; 3] = ["e0", "e1", "e2"];
const RULE_ETYPE: &str = "OV";
const KEY_MOD: u8 = 16;

#[derive(Debug, Clone)]
enum Op {
    InsertNode(u8),
    InsertEdge(u8, u8, u8),
    SetScalar(u8, i8),
    SetTags(u8, u8, u8),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => any::<u8>().prop_map(Op::InsertNode),
        2 => (any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(t, s, d)| Op::InsertEdge(t, s, d)),
        2 => (any::<u8>(), any::<i8>()).prop_map(|(k, v)| Op::SetScalar(k, v)),
        2 => (any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(k, t1, t2)| Op::SetTags(k, t1, t2)),
    ]
}

fn key_of(n: u8) -> String {
    format!("k{}", n % KEY_MOD)
}

fn tags_of(t1: u8, t2: u8) -> Value {
    let mut items = vec![
        Value::Str(TAGS[(t1 as usize) % TAGS.len()].into()),
        Value::Str(TAGS[(t2 as usize) % TAGS.len()].into()),
    ];
    items.dedup();
    Value::List(items)
}

fn scored_overlap() -> RuleDef {
    RuleDef {
        name: "ov_scored".into(),
        src_label: "L0".into(),
        dst_label: "L1".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.3,
        },
        edge_type: RULE_ETYPE.into(),
        weight_prop: Some("score".into()),
    }
}

fn apply_op(db: &mut GraphDb<SimFs>, live: &mut BTreeSet<String>, op: &Op) {
    match op {
        Op::InsertNode(n) => {
            let key = key_of(*n);
            let label = format!("L{}", n % 2);
            let props = vec![
                ("k".into(), Value::Str(key.clone())),
                ("p".into(), Value::Int(i64::from(*n))),
                ("tags".into(), tags_of(*n, n.wrapping_add(1))),
            ];
            if db.insert_node(&label, &key, props).is_ok() {
                live.insert(key);
            }
        }
        Op::InsertEdge(t, s, d) => {
            let etype = USER_ETYPES[(*t as usize) % USER_ETYPES.len()];
            let _ = db.insert_edge(etype, &key_of(*s), &key_of(*d));
        }
        Op::SetScalar(k, v) => {
            let _ = db.set_prop(&key_of(*k), "p", Value::Int(i64::from(*v)));
        }
        Op::SetTags(k, t1, t2) => {
            let _ = db.set_prop(&key_of(*k), "tags", tags_of(*t1, *t2));
        }
    }
}

fn params_for(key: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("key".into(), Value::Str(key.into()));
    p
}

fn row_key_set(rs: &core_api::ResultSet, col: &str) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    for i in 0..rs.len() {
        match rs.get(i, col) {
            Some(Value::Str(s)) => {
                out.insert(s.clone());
            }
            other => {
                return Err(format!(
                    "row {i} column {col:?} is not a node key: {other:?}"
                ));
            }
        }
    }
    Ok(out)
}

fn etypes_in_play() -> Vec<&'static str> {
    let mut v: Vec<&str> = USER_ETYPES.to_vec();
    v.push(RULE_ETYPE);
    v
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn cypher_matches_traversal(
        ops in proptest::collection::vec(op_strategy(), 1..60),
        rule_slot in any::<u8>(),
    ) {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        let mut live = BTreeSet::new();

        let n = ops.len();
        let rule_at = if n <= 1 {
            0
        } else {
            1 + (rule_slot as usize % (n - 1))
        };

        let mut rule_created = false;
        for (i, op) in ops.iter().enumerate() {
            if i == rule_at {
                db.create_rule(scored_overlap())
                    .expect("single scored Overlap rule");
                rule_created = true;
            }
            apply_op(&mut db, &mut live, op);
        }
        if !rule_created {
            db.create_rule(scored_overlap())
                .expect("single scored Overlap rule");
        }

        // Empty==empty is allowed here when Overlap does not fire. The
        // derived-edge path is pinned non-vacuous by
        // `overlap_rule_equality_on_nonempty_derived_sets`.
        let sample: Vec<String> = live.iter().take(8).cloned().collect();
        let etypes = etypes_in_play();

        for key in &sample {
            let params = params_for(key);

            for etype in &etypes {
                // (a) directed Out Cypher == neighbors(key, etype, Out)
                let directed = format!("MATCH (a {{k: $key}})-[r:{etype}]->(b) RETURN b");
                let qres = db.query(&directed, &params);
                prop_assert!(
                    qres.is_ok(),
                    "directed query Err (engine bug)\n  q={}\n  key={}\n  rule_at={}\n  ops={:?}\n  err={:?}",
                    directed,
                    key,
                    rule_at,
                    ops,
                    qres.as_ref().err()
                );
                let cypher_out = row_key_set(&qres.unwrap(), "b").map_err(|e| {
                    TestCaseError::fail(format!(
                        "directed row set (engine bug)\n  q={directed}\n  key={key}\n  rule_at={rule_at}\n  ops={ops:?}\n  {e}"
                    ))
                })?;
                let neigh = db.neighbors(key, etype, Direction::Out);
                prop_assert!(
                    neigh.is_ok(),
                    "neighbors Err (engine bug)\n  key={} etype={}\n  rule_at={}\n  ops={:?}\n  err={:?}",
                    key,
                    etype,
                    rule_at,
                    ops,
                    neigh.as_ref().err()
                );
                let trav_out: BTreeSet<String> = neigh.unwrap().into_iter().collect();
                prop_assert_eq!(
                    &cypher_out,
                    &trav_out,
                    "directed set mismatch (engine bug)\n  q={}\n  key={} etype={}\n  rule_at={}\n  ops={:?}\n  cypher={:?}\n  neighbors={:?}",
                    directed,
                    key,
                    etype,
                    rule_at,
                    ops,
                    cypher_out,
                    trav_out
                );

                // (b) 1-hop undirected Cypher == grouped_by_edge_type bucket
                let undirected = format!("MATCH (a {{k: $key}})-[r:{etype}]-(b) RETURN b");
                let ures = db.query(&undirected, &params);
                prop_assert!(
                    ures.is_ok(),
                    "undirected query Err (engine bug)\n  q={}\n  key={}\n  rule_at={}\n  ops={:?}\n  err={:?}",
                    undirected,
                    key,
                    rule_at,
                    ops,
                    ures.as_ref().err()
                );
                let cypher_both = row_key_set(&ures.unwrap(), "b").map_err(|e| {
                    TestCaseError::fail(format!(
                        "undirected row set (engine bug)\n  q={undirected}\n  key={key}\n  rule_at={rule_at}\n  ops={ops:?}\n  {e}"
                    ))
                })?;
                let node = db.node_ref(key);
                prop_assert!(
                    node.is_some(),
                    "node_ref missing live key {}\n  rule_at={}\n  ops={:?}",
                    key,
                    rule_at,
                    ops
                );
                let grouped = node.unwrap().grouped_by_edge_type();
                let bucket: BTreeSet<String> = grouped
                    .get(*etype)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                prop_assert_eq!(
                    &cypher_both,
                    &bucket,
                    "undirected set mismatch (engine bug)\n  q={}\n  key={} etype={}\n  rule_at={}\n  ops={:?}\n  cypher={:?}\n  grouped={:?}",
                    undirected,
                    key,
                    etype,
                    rule_at,
                    ops,
                    cypher_both,
                    bucket
                );
            }
        }
    }
}

/// Guaranteed Overlap fire: L0/L1 share tags (Jaccard 1.0 ≥ 0.3). Asserts
/// Cypher↔traversal equality on **non-empty** OV sets so a silent drop of
/// all derived edges cannot pass.
#[test]
fn overlap_rule_equality_on_nonempty_derived_sets() {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();
    db.insert_node(
        "L0",
        "src",
        vec![
            ("k".into(), Value::Str("src".into())),
            ("p".into(), Value::Int(0)),
            ("tags".into(), tags_of(0, 1)),
        ],
    )
    .unwrap();
    db.insert_node(
        "L1",
        "mid",
        vec![
            ("k".into(), Value::Str("mid".into())),
            ("p".into(), Value::Int(1)),
            ("tags".into(), tags_of(2, 2)),
        ],
    )
    .unwrap();
    db.create_rule(scored_overlap()).unwrap();
    db.insert_node(
        "L1",
        "dst",
        vec![
            ("k".into(), Value::Str("dst".into())),
            ("p".into(), Value::Int(2)),
            ("tags".into(), tags_of(0, 1)),
        ],
    )
    .unwrap();
    db.insert_edge("e0", "src", "dst").unwrap();

    let params = params_for("src");
    let directed = format!("MATCH (a {{k: $key}})-[r:{RULE_ETYPE}]->(b) RETURN b");
    let cypher_out = row_key_set(&db.query(&directed, &params).expect("directed OV"), "b")
        .expect("directed OV keys");
    let trav_out: BTreeSet<String> = db
        .neighbors("src", RULE_ETYPE, Direction::Out)
        .expect("neighbors OV")
        .into_iter()
        .collect();
    assert!(
        !cypher_out.is_empty(),
        "Overlap must produce a non-empty directed OV set; cypher={cypher_out:?} neighbors={trav_out:?}"
    );
    assert_eq!(cypher_out, trav_out);

    let undirected = format!("MATCH (a {{k: $key}})-[r:{RULE_ETYPE}]-(b) RETURN b");
    let cypher_both = row_key_set(&db.query(&undirected, &params).expect("undirected OV"), "b")
        .expect("undirected OV keys");
    let grouped = db.node_ref("src").expect("src").grouped_by_edge_type();
    let bucket: BTreeSet<String> = grouped
        .get(RULE_ETYPE)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    assert!(
        !cypher_both.is_empty(),
        "Overlap must produce a non-empty undirected OV set; cypher={cypher_both:?} grouped={bucket:?}"
    );
    assert_eq!(cypher_both, bucket);
    assert!(
        cypher_out.contains("dst"),
        "derived OV must include dst; got {cypher_out:?}"
    );
}
