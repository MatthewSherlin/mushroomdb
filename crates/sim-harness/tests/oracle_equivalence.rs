use core_api::{Direction, GraphDb, GraphError, Predicate, RuleDef, Value};
use proptest::prelude::*;
use sim_harness::{Oracle, SimFs};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Fixed rule template pool (4 templates, distinct names and edge types)
// ---------------------------------------------------------------------------

const RULE_NAMES: [&str; 4] = ["r_km", "r_fe", "r_ov", "r_all"];
const RULE_ETYPES: [&str; 4] = ["r_km", "r_fe", "r_ov", "r_all"];
const USER_ETYPES: [&str; 3] = ["e0", "e1", "e2"];

/// 3-token alphabet used by SetTags.
const TAGS_ALPHA: [&str; 3] = ["a", "b", "c"];

fn rule_template(idx: u8) -> RuleDef {
    match idx % 4 {
        0 => RuleDef {
            name: "r_km".into(),
            src_label: "L0".into(),
            dst_label: "L1".into(),
            predicate: Predicate::KeyMatch { field: "f".into() },
            edge_type: "r_km".into(),
            weight_prop: None,
        },
        1 => RuleDef {
            name: "r_fe".into(),
            src_label: "L0".into(),
            dst_label: "L0".into(),
            predicate: Predicate::FieldEqual { field: "f".into() },
            edge_type: "r_fe".into(),
            weight_prop: None,
        },
        2 => RuleDef {
            name: "r_ov".into(),
            src_label: "L1".into(),
            dst_label: "L1".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.34,
            },
            edge_type: "r_ov".into(),
            weight_prop: None,
        },
        _ => RuleDef {
            name: "r_all".into(),
            src_label: "L0".into(),
            dst_label: "L0".into(),
            predicate: Predicate::All(vec![
                Predicate::FieldEqual { field: "f".into() },
                Predicate::Overlap {
                    field: "tags".into(),
                    min: 0.34,
                },
            ]),
            edge_type: "r_all".into(),
            weight_prop: None,
        },
    }
}

fn all_etypes() -> Vec<String> {
    USER_ETYPES
        .iter()
        .chain(RULE_ETYPES.iter())
        .map(|s| s.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Edge set sweep helper
// ---------------------------------------------------------------------------

fn sweep_engine_edges(db: &GraphDb<SimFs>) -> BTreeSet<(String, String, String)> {
    let mut out = BTreeSet::new();
    let etypes = all_etypes();
    for n in 0..=255u8 {
        let key = format!("k{n}");
        for etype in &etypes {
            for dir in [Direction::Out, Direction::In] {
                for neighbor in db.neighbors(&key, etype, dir).unwrap_or_default() {
                    let (src, dst) = match dir {
                        Direction::Out => (key.clone(), neighbor),
                        Direction::In => (neighbor, key.clone()),
                    };
                    out.insert((etype.clone(), src, dst));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Op enum and strategy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    InsertNode(u8),          // key = "k{n}", label = "L{n%2}"
    InsertEdge(u8, u8, u8),  // etype index 0-6 (0-2: user e{i}, 3-6: rule etypes); src k; dst k
    SetProp(u8, u8),         // key, int value → writes "p"
    SetF(u8, u8),            // key, target_key_index → writes "f" = "k{m}"
    SetTags(u8, u8, u8),     // key, tok1, tok2 → writes "tags" as Value::List from 3-token alphabet
    CreateRule(u8),          // picks from the 4 templates by index
    DeleteRule(u8),          // picks from the 4 rule names by index
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u8>().prop_map(Op::InsertNode),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(t, s, d)| Op::InsertEdge(t, s, d)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetProp(k, v)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, m)| Op::SetF(k, m)),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(k, t1, t2)| Op::SetTags(k, t1, t2)),
        any::<u8>().prop_map(Op::CreateRule),
        any::<u8>().prop_map(Op::DeleteRule),
    ]
}

// ---------------------------------------------------------------------------
// Proptest equivalence suite
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn engine_matches_oracle(ops in proptest::collection::vec(op_strategy(), 1..120)) {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        let mut oracle = Oracle::new();

        for op in &ops {
            match op {
                Op::InsertNode(n) => {
                    let key = format!("k{n}");
                    let label = format!("L{}", n % 2);
                    let props = vec![("seed".to_string(), Value::Int(*n as i64))];
                    let db_ok = db.insert_node(&label, &key, props.clone()).is_ok();
                    let or_ok = oracle.insert_node(&label, &key, &props);
                    prop_assert_eq!(db_ok, or_ok);
                }

                Op::InsertEdge(t, s, d) => {
                    // etype index 0..7: 0-2 are user etypes (e0/e1/e2), 3-6 are rule etypes.
                    let etype_idx = (*t as usize) % 7;
                    let etype: String = match etype_idx {
                        0 | 1 | 2 => format!("e{etype_idx}"),
                        3 => "r_km".into(),
                        4 => "r_fe".into(),
                        5 => "r_ov".into(),
                        _ => "r_all".into(),
                    };
                    let src = format!("k{s}");
                    let dst = format!("k{d}");

                    // Oracle pre-checks: used to categorise the expected engine result.
                    let both_exist = oracle.has_node(&src) && oracle.has_node(&dst);
                    let already_user = oracle.has_user_edge(&etype, &src, &dst);
                    let oracle_derived = both_exist && oracle.is_derived_edge(&etype, &src, &dst);
                    // "True" ownership: derived AND no pre-existing user edge for this triple.
                    // A user edge inserted before the rule fired is not owned; the engine
                    // returns Ok(false) (duplicate) rather than Err(RuleOwned).
                    let rule_owned = oracle_derived && !already_user;

                    let db_res = db.insert_edge(&etype, &src, &dst);
                    match &db_res {
                        Err(GraphError::KeyNotFound { .. }) => {
                            prop_assert!(
                                !both_exist,
                                "engine returned KeyNotFound but oracle has both nodes; \
                                 etype={etype} src={src} dst={dst}"
                            );
                        }
                        Err(GraphError::RuleOwned { .. }) => {
                            prop_assert!(
                                rule_owned,
                                "engine returned RuleOwned but oracle does not see a \
                                 rule-owned pair; etype={etype} src={src} dst={dst} \
                                 derived={oracle_derived} already_user={already_user}"
                            );
                        }
                        Ok(v) => {
                            prop_assert!(
                                !rule_owned,
                                "engine returned Ok({v}) but oracle sees a rule-owned pair; \
                                 etype={etype} src={src} dst={dst}"
                            );
                            // Agree on insertion outcome (true=new, false=duplicate).
                            let or_v = oracle.insert_edge(&etype, &src, &dst);
                            prop_assert_eq!(
                                Some(*v),
                                or_v,
                                "insert_edge Ok result mismatch; etype={} src={} dst={}",
                                etype,
                                src,
                                dst
                            );
                        }
                        Err(e) => {
                            prop_assert!(
                                false,
                                "insert_edge returned unexpected error: {e:?}; \
                                 etype={etype} src={src} dst={dst}"
                            );
                        }
                    }
                }

                Op::SetProp(k, v) => {
                    let key = format!("k{k}");
                    let db_ok = db.set_prop(&key, "p", Value::Int(*v as i64)).is_ok();
                    let or_ok = oracle.set_prop(&key, "p", Value::Int(*v as i64));
                    prop_assert_eq!(db_ok, or_ok);
                }

                Op::SetF(k, m) => {
                    // Writes "f" = "k{m}" (a node-key string), giving KeyMatch and FieldEqual
                    // rules matching material.
                    let key = format!("k{k}");
                    let val = Value::Str(format!("k{m}"));
                    let db_ok = db.set_prop(&key, "f", val.clone()).is_ok();
                    let or_ok = oracle.set_prop(&key, "f", val);
                    prop_assert_eq!(db_ok, or_ok);
                }

                Op::SetTags(k, t1, t2) => {
                    // Writes "tags" as a Value::List drawn from TAGS_ALPHA; dedup is fine.
                    let key = format!("k{k}");
                    let mut tag_vals: Vec<Value> = [t1, t2]
                        .iter()
                        .map(|&t| Value::Str(TAGS_ALPHA[(*t as usize) % 3].into()))
                        .collect();
                    tag_vals.dedup();
                    let val = Value::List(tag_vals);
                    let db_ok = db.set_prop(&key, "tags", val.clone()).is_ok();
                    let or_ok = oracle.set_prop(&key, "tags", val);
                    prop_assert_eq!(db_ok, or_ok);
                }

                Op::CreateRule(n) => {
                    let def = rule_template(*n);
                    let db_ok = db.create_rule(def.clone()).is_ok();
                    let or_ok = oracle.create_rule(def);
                    prop_assert_eq!(
                        db_ok,
                        or_ok,
                        "create_rule result mismatch for template {}",
                        n % 4
                    );
                }

                Op::DeleteRule(n) => {
                    let name = RULE_NAMES[(*n as usize) % 4];
                    let db_ok = db.delete_rule(name).is_ok();
                    let or_ok = oracle.delete_rule(name);
                    prop_assert_eq!(
                        db_ok,
                        or_ok,
                        "delete_rule result mismatch for rule {}",
                        name
                    );
                }
            }
        }

        // --- Full-state comparison ---

        prop_assert_eq!(db.node_count(), oracle.node_count());

        // Prop sweeps: seed (InsertNode), p (SetProp), f (SetF), tags (SetTags).
        for n in 0..=255u8 {
            let key = format!("k{n}");
            for field in &["seed", "p", "f", "tags"] {
                prop_assert_eq!(
                    db.get_prop(&key, field),
                    oracle.get_prop(&key, field),
                    "prop mismatch key={} field={}",
                    key,
                    field
                );
            }
        }

        // Req 5: engine full edge set (user ∪ derived) == oracle.all_edges().
        // Engine edge set is built by sweeping neighbors for all 256 keys × all 7 edge
        // types (e0/e1/e2 + r_km/r_fe/r_ov/r_all) × both directions, then deduplicating
        // Out/In entries into a single (etype, src, dst) set.
        let engine_edges = sweep_engine_edges(&db);
        let oracle_edges = oracle.all_edges();
        prop_assert_eq!(
            &engine_edges,
            &oracle_edges,
            "final edge set mismatch (engine vs oracle)"
        );

        // Req 6: rebuild-is-noop invariant.
        // For every live rule, rebuild it and re-sweep; the edge set must not change.
        let rules_snapshot = db.rules();
        for rule in &rules_snapshot {
            db.rebuild_rule(&rule.name).unwrap();
            let edges_after = sweep_engine_edges(&db);
            prop_assert_eq!(
                &engine_edges,
                &edges_after,
                "rebuild_rule({}) changed the edge set",
                rule.name
            );
        }
    }
}
