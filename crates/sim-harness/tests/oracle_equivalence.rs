use core_api::{
    AggFn, Direction, GraphDb, GraphError, Predicate, RuleDef, Value, ViewDef, ViewSource,
};
use proptest::prelude::*;
use sim_harness::{Oracle, SimFs, APPROX_RECALL_FLOOR_QUIESCED, APPROX_RECALL_FLOOR_RECOVERY};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Fixed rule template pool (4 templates, distinct names and edge types)
// ---------------------------------------------------------------------------

// r_ov2 shares edge_type "r_fe" with r_fe (C1 coverage: co-owned edge type survival).
const N_TEMPLATES: usize = 10;
const RULE_NAMES: [&str; 10] = [
    "r_km", "r_fe", "r_ov", "r_all", "r_ov2", "r_nw", "r_nz", "r_geo", "r_vec", "r_via",
];
const RULE_ETYPES: [&str; 9] = [
    "r_km", "r_fe", "r_ov", "r_all", "r_nw", "r_nz", "r_geo", "r_vec", "r_via",
];
const USER_ETYPES: [&str; 3] = ["e0", "e1", "e2"];

/// 3-token alphabet used by SetTags.
const TAGS_ALPHA: [&str; 3] = ["a", "b", "c"];

/// Fulltext label pool: match the InsertNode labels.
const FT_LABELS: [&str; 2] = ["L0", "L1"];

/// Fulltext field pool. Both fields produce real postings:
/// "f" is a Str (SetF writes Value::Str("k{n}")), exercising Str tokenization.
/// "tags" is a List<Str> drawn from TAGS_ALPHA, exercising List tokenization.
const FT_FIELDS: [&str; 2] = ["f", "tags"];

/// Fulltext query pool covering both field types:
/// "k0" / "k*" match "f" values; "a" / "a OR b" match "tags" values.
const FT_QUERIES: [&str; 4] = ["k0", "k*", "a", "a OR b"];

fn ft_label(n: u8) -> &'static str {
    FT_LABELS[(n as usize) % FT_LABELS.len()]
}

fn ft_field(n: u8) -> &'static str {
    FT_FIELDS[(n as usize) % FT_FIELDS.len()]
}

fn ft_query(n: u8) -> &'static str {
    FT_QUERIES[(n as usize) % FT_QUERIES.len()]
}

fn rule_template(idx: u8) -> RuleDef {
    match idx as usize % N_TEMPLATES {
        0 => RuleDef {
            name: "r_km".into(),
            src_label: "L0".into(),
            dst_label: "L1".into(),
            predicate: Predicate::KeyMatch { field: "f".into() },
            edge_type: "r_km".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        },
        1 => RuleDef {
            name: "r_fe".into(),
            src_label: "L0".into(),
            dst_label: "L0".into(),
            predicate: Predicate::FieldEqual { field: "f".into() },
            edge_type: "r_fe".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
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
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        },
        3 => RuleDef {
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
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        },
        // Template 4: shares edge_type "r_fe" with r_fe — exercises C1 (co-owned
        // edge type survival after rule deletion).  Different name and lower min
        // than r_fe's FieldEqual predicate, so it can derive edges r_fe cannot.
        4 => RuleDef {
            name: "r_ov2".into(),
            src_label: "L0".into(),
            dst_label: "L0".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.1,
            },
            edge_type: "r_fe".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        },
        5 => RuleDef {
            name: "r_nw".into(),
            src_label: "L0".into(),
            dst_label: "L0".into(),
            predicate: Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 2.0,
            },
            edge_type: "r_nw".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        },
        6 => RuleDef {
            name: "r_nz".into(),
            src_label: "L0".into(),
            dst_label: "L0".into(),
            predicate: Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 0.0,
            },
            edge_type: "r_nz".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        },
        7 => RuleDef {
            name: "r_geo".into(),
            src_label: "L1".into(),
            dst_label: "L1".into(),
            predicate: Predicate::GeoRadius {
                field: "loc".into(),
                km: 400.0,
            },
            edge_type: "r_geo".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        },
        8 => RuleDef {
            name: "r_vec".into(),
            src_label: "L0".into(),
            dst_label: "L0".into(),
            predicate: Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            },
            edge_type: "r_vec".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        },
        // Template 9: via-hop rule. Semantics: L0 -[e0]-> L1, FieldEqual(f)
        // between L1(via) and L0(dst), fire r_via edge src→dst.
        // src_label == dst_label == "L0" intentionally exercises the self-edge
        // guard: the oracle and engine must both skip src == dst pairs.
        _ => RuleDef {
            name: "r_via".into(),
            src_label: "L0".into(),
            dst_label: "L0".into(),
            predicate: Predicate::FieldEqual { field: "f".into() },
            edge_type: "r_via".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: Some("L1".into()),
            via_edge: Some("e0".into()),
            via_dir: None,
        },
    }
}

// ---------------------------------------------------------------------------
// View template pool (3 templates; view_prop names distinct from prop_fields)
// ---------------------------------------------------------------------------

const N_VIEW_TEMPLATES: usize = 3;
const VIEW_NAMES: [&str; N_VIEW_TEMPLATES] = ["v_deg0", "v_sum1", "v_cnt2"];

fn view_template(idx: u8) -> ViewDef {
    match (idx as usize) % N_VIEW_TEMPLATES {
        0 => ViewDef {
            name: "v_deg0".into(),
            label: "L0".into(),
            view_prop: "v_deg0".into(),
            source: ViewSource::Degree {
                edge_type: "e0".into(),
                direction: Direction::Out,
            },
        },
        1 => ViewDef {
            name: "v_sum1".into(),
            label: "L1".into(),
            view_prop: "v_sum1".into(),
            source: ViewSource::NeighborAgg {
                edge_type: "e1".into(),
                direction: Direction::In,
                agg: AggFn::Sum,
                prop: "p".into(),
            },
        },
        _ => ViewDef {
            name: "v_cnt2".into(),
            label: "L0".into(),
            view_prop: "v_cnt2".into(),
            source: ViewSource::NeighborAgg {
                edge_type: "e2".into(),
                direction: Direction::Out,
                agg: AggFn::Count,
                prop: "seed".into(),
            },
        },
    }
}

fn loc_list(lat: f64, lon: f64) -> Value {
    Value::List(vec![Value::Float(lat), Value::Float(lon)])
}

fn emb_list(xs: &[f64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Float).collect())
}

/// Bucket-crossing years plus signed zero. Indexed by `n/2` so both L0
/// (even n) values include −0.0 and +0.0.
fn year_val(n: u8) -> Value {
    match (n / 2) % 6 {
        0 => Value::Float(-0.0),
        1 => Value::Float(0.0),
        2 => Value::Float(10.0),
        3 => Value::Float(11.9),
        4 => Value::Float(12.0),
        _ => Value::Float(16.1),
    }
}

fn loc_val(n: u8) -> Value {
    match n % 5 {
        0 => loc_list(48.8566, 2.3522),
        1 => loc_list(51.5074, -0.1278),
        2 => loc_list(70.0, 179.9),
        3 => loc_list(70.0, -179.9),
        _ => loc_list(40.7128, -74.0060),
    }
}

fn emb_val(n: u8) -> Value {
    match n % 4 {
        0 => emb_list(&[1.0, 0.0]),
        1 => emb_list(&[0.95, (1.0_f64 - 0.95 * 0.95).sqrt()]),
        2 => emb_list(&[0.0, 1.0]),
        _ => emb_list(&[0.5, 0.5]),
    }
}

fn insert_node_props(n: u8) -> Vec<(String, Value)> {
    vec![
        ("seed".to_string(), Value::Int(n as i64)),
        ("year".to_string(), year_val(n)),
        ("loc".to_string(), loc_val(n)),
        ("emb".to_string(), emb_val(n)),
    ]
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

fn engine_weight(db: &GraphDb<SimFs>, src: &str, dst: &str, etype: &str) -> Option<f64> {
    db.explain(src, dst)
        .ok()?
        .into_iter()
        .find(|e| e.edge_type == etype && e.src_key == src && e.dst_key == dst)
        .and_then(|e| e.weight)
}

fn assert_weights_match(db: &GraphDb<SimFs>, oracle: &Oracle) -> Result<(), String> {
    for ((et, s, d), want) in oracle.derived_weights() {
        if let Some(got) = engine_weight(db, &s, &d, &et) {
            if (got - want).abs() >= 1e-9 {
                return Err(format!("weight {et} {s}->{d}: engine {got} oracle {want}"));
            }
        }
    }
    Ok(())
}

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

const PROP_FIELDS: [&str; 7] = ["seed", "p", "f", "tags", "year", "loc", "emb"];

#[derive(Debug, Clone)]
enum Op {
    InsertNode(u8),          // key = "k{n}", label = "L{n%2}"
    InsertEdge(u8, u8, u8),  // etype index 0-6 (0-2: user e{i}, 3-6: rule etypes); src k; dst k
    SetProp(u8, u8),         // key, int value → writes "p"
    SetF(u8, u8),            // key, target_key_index → writes "f" = "k{m}"
    SetTags(u8, u8, u8),     // key, tok1, tok2 → writes "tags" as Value::List from 3-token alphabet
    CreateRule(u8),          // picks from the 5 templates by index
    DeleteRule(u8),          // picks from the 5 rule names by index
    DeleteNode(u8),          // key = "k{n}"
    DeleteEdge(u8, u8, u8),  // etype index, src k, dst k
    RemoveProp(u8, u8),      // key, field-selector → PROP_FIELDS[sel % 7]
    SetYear(u8, u8),         // key, year-selector → bucket / signed-zero values
    SetLoc(u8, u8),          // key, loc-selector → Paris/London/±180/NYC
    SetEmb(u8, u8),          // key, emb-selector → near-threshold / orthogonal
    CreateView(u8),          // pick from N_VIEW_TEMPLATES templates by index
    DeleteView(u8),          // pick from VIEW_NAMES by index
    EnableFulltext(u8, u8),  // label idx (0→"L0", 1→"L1"), field idx into FT_FIELDS
    DisableFulltext(u8, u8), // label idx, field idx
    FulltextSearch(u8, u8),  // field idx, query idx — asserts db.search == oracle.scratch_search
    /// 2–4 leaf ops committed as one engine `batch()`. Nested Batch is never
    /// generated. CreateRule is omitted from the inner pool so we do not hit
    /// the documented same-batch rule-window (validation cannot see edges a
    /// CreateRule in this batch will derive).
    Batch(Vec<Op>),
}

fn etype_of(t: u8) -> String {
    match (t as usize) % 12 {
        0..=2 => format!("e{}", (t as usize) % 3),
        3 => "r_km".into(),
        4 => "r_fe".into(),
        5 => "r_ov".into(),
        6 => "r_all".into(),
        7 => "r_nw".into(),
        8 => "r_nz".into(),
        9 => "r_geo".into(),
        10 => "r_vec".into(),
        _ => "r_via".into(),
    }
}

fn field_of(sel: u8) -> &'static str {
    PROP_FIELDS[(sel as usize) % PROP_FIELDS.len()]
}

/// Leaf ops only — used both standalone and as Batch contents.
fn leaf_op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u8>().prop_map(Op::InsertNode),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(t, s, d)| Op::InsertEdge(t, s, d)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetProp(k, v)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, m)| Op::SetF(k, m)),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(k, t1, t2)| Op::SetTags(k, t1, t2)),
        any::<u8>().prop_map(Op::CreateRule),
        any::<u8>().prop_map(Op::DeleteRule),
        any::<u8>().prop_map(Op::DeleteNode),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(t, s, d)| Op::DeleteEdge(t, s, d)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, f)| Op::RemoveProp(k, f)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetYear(k, v)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetLoc(k, v)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetEmb(k, v)),
        any::<u8>().prop_map(Op::CreateView),
        any::<u8>().prop_map(Op::DeleteView),
        (any::<u8>(), any::<u8>()).prop_map(|(l, f)| Op::EnableFulltext(l, f)),
        (any::<u8>(), any::<u8>()).prop_map(|(l, f)| Op::DisableFulltext(l, f)),
        (any::<u8>(), any::<u8>()).prop_map(|(f, q)| Op::FulltextSearch(f, q)),
    ]
}

fn batch_inner_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u8>().prop_map(Op::InsertNode),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(t, s, d)| Op::InsertEdge(t, s, d)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetProp(k, v)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, m)| Op::SetF(k, m)),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(k, t1, t2)| Op::SetTags(k, t1, t2)),
        any::<u8>().prop_map(Op::DeleteRule),
        any::<u8>().prop_map(Op::DeleteNode),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(t, s, d)| Op::DeleteEdge(t, s, d)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, f)| Op::RemoveProp(k, f)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetYear(k, v)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetLoc(k, v)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetEmb(k, v)),
    ]
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        leaf_op_strategy(),
        proptest::collection::vec(batch_inner_strategy(), 2..=4).prop_map(Op::Batch),
    ]
}

/// Apply one leaf op to the oracle. Hard failures (`KeyNotFound` /
/// `RuleOwned` / duplicate / missing rule) return `Err`; no-ops (`Ok(false)`)
/// return `Ok`.
fn apply_oracle_leaf(oracle: &mut Oracle, op: &Op) -> Result<(), String> {
    match op {
        Op::InsertNode(n) => {
            let key = format!("k{n}");
            let label = format!("L{}", n % 2);
            let props = insert_node_props(*n);
            if oracle.insert_node(&label, &key, &props) {
                Ok(())
            } else {
                Err(format!("oracle insert_node({key}) duplicate"))
            }
        }
        Op::InsertEdge(t, s, d) => {
            let etype = etype_of(*t);
            let src = format!("k{s}");
            let dst = format!("k{d}");
            let both = oracle.has_node(&src) && oracle.has_node(&dst);
            let already = oracle.has_user_edge(&etype, &src, &dst);
            let derived = both && oracle.is_derived_edge(&etype, &src, &dst);
            if !both {
                return Err(format!(
                    "oracle insert_edge KeyNotFound {etype} {src}->{dst}"
                ));
            }
            if derived && !already {
                return Err(format!("oracle insert_edge RuleOwned {etype} {src}->{dst}"));
            }
            let _ = oracle.insert_edge(&etype, &src, &dst);
            Ok(())
        }
        Op::SetProp(k, v) => {
            let key = format!("k{k}");
            if oracle.set_prop(&key, "p", Value::Int(*v as i64)) {
                Ok(())
            } else {
                Err(format!("oracle set_prop({key}) KeyNotFound"))
            }
        }
        Op::SetF(k, m) => {
            let key = format!("k{k}");
            if oracle.set_prop(&key, "f", Value::Str(format!("k{m}"))) {
                Ok(())
            } else {
                Err(format!("oracle set_f({key}) KeyNotFound"))
            }
        }
        Op::SetTags(k, t1, t2) => {
            let key = format!("k{k}");
            let mut tag_vals: Vec<Value> = [t1, t2]
                .iter()
                .map(|&t| Value::Str(TAGS_ALPHA[(*t as usize) % 3].into()))
                .collect();
            tag_vals.dedup();
            if oracle.set_prop(&key, "tags", Value::List(tag_vals)) {
                Ok(())
            } else {
                Err(format!("oracle set_tags({key}) KeyNotFound"))
            }
        }
        Op::CreateRule(n) => {
            if oracle.create_rule(rule_template(*n)) {
                Ok(())
            } else {
                Err(format!(
                    "oracle create_rule({}) rejected",
                    n % N_TEMPLATES as u8
                ))
            }
        }
        Op::DeleteRule(n) => {
            let name = RULE_NAMES[(*n as usize) % N_TEMPLATES];
            if oracle.delete_rule(name) {
                Ok(())
            } else {
                Err(format!("oracle delete_rule({name}) missing"))
            }
        }
        Op::DeleteNode(n) => {
            let key = format!("k{n}");
            if oracle.delete_node(&key) {
                Ok(())
            } else {
                Err(format!("oracle delete_node({key}) KeyNotFound"))
            }
        }
        Op::DeleteEdge(t, s, d) => {
            match oracle.delete_edge(&etype_of(*t), &format!("k{s}"), &format!("k{d}")) {
                None => Err(format!(
                    "oracle delete_edge KeyNotFound {} k{s}->k{d}",
                    etype_of(*t)
                )),
                Some(None) => Err(format!(
                    "oracle delete_edge RuleOwned {} k{s}->k{d}",
                    etype_of(*t)
                )),
                Some(Some(_)) => Ok(()),
            }
        }
        Op::RemoveProp(k, f) => match oracle.remove_prop(&format!("k{k}"), field_of(*f)) {
            None => Err(format!(
                "oracle remove_prop(k{k}, {}) KeyNotFound",
                field_of(*f)
            )),
            Some(_) => Ok(()),
        },
        Op::SetYear(k, v) => {
            let key = format!("k{k}");
            if oracle.set_prop(&key, "year", year_val(*v)) {
                Ok(())
            } else {
                Err(format!("oracle set_year({key}) KeyNotFound"))
            }
        }
        Op::SetLoc(k, v) => {
            let key = format!("k{k}");
            if oracle.set_prop(&key, "loc", loc_val(*v)) {
                Ok(())
            } else {
                Err(format!("oracle set_loc({key}) KeyNotFound"))
            }
        }
        Op::SetEmb(k, v) => {
            let key = format!("k{k}");
            if oracle.set_prop(&key, "emb", emb_val(*v)) {
                Ok(())
            } else {
                Err(format!("oracle set_emb({key}) KeyNotFound"))
            }
        }
        Op::CreateView(n) => {
            let def = view_template(*n);
            if oracle.create_view(def) {
                Ok(())
            } else {
                Err(format!(
                    "oracle create_view({}) rejected",
                    n % N_VIEW_TEMPLATES as u8
                ))
            }
        }
        Op::DeleteView(n) => {
            let name = VIEW_NAMES[(*n as usize) % N_VIEW_TEMPLATES];
            if oracle.delete_view(name) {
                Ok(())
            } else {
                Err(format!("oracle delete_view({name}) missing"))
            }
        }
        Op::EnableFulltext(l, f) => {
            let label = ft_label(*l);
            let field = ft_field(*f);
            if oracle.enable_fulltext(label, field) {
                Ok(())
            } else {
                Err(format!(
                    "oracle enable_fulltext({label},{field}) already enabled"
                ))
            }
        }
        Op::DisableFulltext(l, f) => {
            let label = ft_label(*l);
            let field = ft_field(*f);
            if oracle.disable_fulltext(label, field) {
                Ok(())
            } else {
                Err(format!(
                    "oracle disable_fulltext({label},{field}) not enabled"
                ))
            }
        }
        Op::FulltextSearch(_, _) => Ok(()), // read-only; assertion is in engine loop
        Op::Batch(_) => Err("nested Batch is invalid".into()),
    }
}

fn queue_batch_op(b: &mut core_api::BatchBuilder<'_, SimFs>, op: &Op) {
    match op {
        Op::InsertNode(n) => {
            let key = format!("k{n}");
            let label = format!("L{}", n % 2);
            b.insert_node(&label, &key, insert_node_props(*n));
        }
        Op::InsertEdge(t, s, d) => {
            b.insert_edge(&etype_of(*t), &format!("k{s}"), &format!("k{d}"));
        }
        Op::SetProp(k, v) => {
            b.set_prop(&format!("k{k}"), "p", Value::Int(*v as i64));
        }
        Op::SetF(k, m) => {
            b.set_prop(&format!("k{k}"), "f", Value::Str(format!("k{m}")));
        }
        Op::SetTags(k, t1, t2) => {
            let mut tag_vals: Vec<Value> = [t1, t2]
                .iter()
                .map(|&t| Value::Str(TAGS_ALPHA[(*t as usize) % 3].into()))
                .collect();
            tag_vals.dedup();
            b.set_prop(&format!("k{k}"), "tags", Value::List(tag_vals));
        }
        Op::CreateRule(_) => {
            // batch_inner_strategy never emits CreateRule (T5 same-batch
            // rule-window); standalone CreateRule is applied outside this fn.
            unreachable!("CreateRule is not a Batch inner op");
        }
        Op::CreateView(_) | Op::DeleteView(_) => {
            // batch_inner_strategy never emits CreateView/DeleteView;
            // view mutations are standalone ops applied outside this fn.
            unreachable!("CreateView/DeleteView are not Batch inner ops");
        }
        Op::EnableFulltext(..) | Op::DisableFulltext(..) | Op::FulltextSearch(..) => {
            // batch_inner_strategy never emits fulltext ops — they are standalone.
            unreachable!("fulltext ops are not Batch inner ops");
        }
        Op::DeleteRule(n) => {
            b.delete_rule(RULE_NAMES[(*n as usize) % N_TEMPLATES]);
        }
        Op::DeleteNode(n) => {
            b.delete_node(&format!("k{n}"));
        }
        Op::DeleteEdge(t, s, d) => {
            b.delete_edge(&etype_of(*t), &format!("k{s}"), &format!("k{d}"));
        }
        Op::RemoveProp(k, f) => {
            b.remove_prop(&format!("k{k}"), field_of(*f));
        }
        Op::SetYear(k, v) => {
            b.set_prop(&format!("k{k}"), "year", year_val(*v));
        }
        Op::SetLoc(k, v) => {
            b.set_prop(&format!("k{k}"), "loc", loc_val(*v));
        }
        Op::SetEmb(k, v) => {
            b.set_prop(&format!("k{k}"), "emb", emb_val(*v));
        }
        Op::Batch(_) => {}
    }
}

// ---------------------------------------------------------------------------
// Proptest equivalence suite
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn engine_matches_oracle(ops in proptest::collection::vec(op_strategy(), 1..80)) {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        let mut oracle = Oracle::new();

        for op in &ops {
            match op {
                Op::InsertNode(n) => {
                    let key = format!("k{n}");
                    let label = format!("L{}", n % 2);
                    let props = insert_node_props(*n);
                    let db_ok = db.insert_node(&label, &key, props.clone()).is_ok();
                    let or_ok = oracle.insert_node(&label, &key, &props);
                    prop_assert_eq!(db_ok, or_ok);
                }

                Op::InsertEdge(t, s, d) => {
                    let etype = etype_of(*t);
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
                        n % N_TEMPLATES as u8
                    );
                }

                Op::DeleteRule(n) => {
                    let name = RULE_NAMES[(*n as usize) % N_TEMPLATES];
                    let db_ok = db.delete_rule(name).is_ok();
                    let or_ok = oracle.delete_rule(name);
                    prop_assert_eq!(
                        db_ok,
                        or_ok,
                        "delete_rule result mismatch for rule {}",
                        name
                    );
                }

                Op::DeleteNode(n) => {
                    let key = format!("k{n}");
                    let db_res = db.delete_node(&key);
                    let or_ok = oracle.delete_node(&key);
                    match db_res {
                        Ok(_report) => prop_assert!(or_ok, "engine Ok delete_node({key}) but oracle KeyNotFound"),
                        Err(GraphError::KeyNotFound { .. }) => {
                            prop_assert!(!or_ok, "engine KeyNotFound delete_node({key}) but oracle deleted")
                        }
                        Err(e) => prop_assert!(false, "delete_node({key}) unexpected error: {e:?}"),
                    }
                }

                Op::DeleteEdge(t, s, d) => {
                    let etype = etype_of(*t);
                    let src = format!("k{s}");
                    let dst = format!("k{d}");
                    let both_exist = oracle.has_node(&src) && oracle.has_node(&dst);
                    // A live rule that would derive this pair → RuleOwned (the
                    // rule would just put the edge back). Matches engine
                    // `is_owned` for provenance-held edges, and also the
                    // user-first + later-rule case the plan rationale names.
                    let rule_owned = both_exist && oracle.is_derived_edge(&etype, &src, &dst);
                    let db_res = db.delete_edge(&etype, &src, &dst);
                    match &db_res {
                        Err(GraphError::KeyNotFound { .. }) => {
                            prop_assert!(
                                !both_exist,
                                "engine KeyNotFound delete_edge but oracle has both; \
                                 etype={etype} src={src} dst={dst}"
                            );
                        }
                        Err(GraphError::RuleOwned { .. }) => {
                            prop_assert!(
                                rule_owned,
                                "engine RuleOwned delete_edge but oracle does not see \
                                 a derived pair; etype={etype} src={src} dst={dst}"
                            );
                        }
                        Ok(v) => {
                            prop_assert!(
                                !rule_owned,
                                "engine Ok({v}) delete_edge but oracle sees a derived pair; \
                                 etype={etype} src={src} dst={dst}"
                            );
                            let or_v = oracle.delete_edge(&etype, &src, &dst);
                            prop_assert_eq!(
                                Some(Some(*v)),
                                or_v,
                                "delete_edge Ok result mismatch; etype={} src={} dst={}",
                                etype,
                                src,
                                dst
                            );
                        }
                        Err(e) => {
                            prop_assert!(
                                false,
                                "delete_edge unexpected error: {e:?}; etype={etype} src={src} dst={dst}"
                            );
                        }
                    }
                }

                Op::RemoveProp(k, f) => {
                    let key = format!("k{k}");
                    let field = field_of(*f);
                    let db_res = db.remove_prop(&key, field);
                    match db_res {
                        Ok(v) => {
                            let or_v = oracle.remove_prop(&key, field);
                            prop_assert_eq!(
                                Some(v),
                                or_v,
                                "remove_prop Ok mismatch key={} field={}",
                                key,
                                field
                            );
                        }
                        Err(GraphError::KeyNotFound { .. }) => {
                            prop_assert_eq!(
                                oracle.remove_prop(&key, field),
                                None,
                                "engine KeyNotFound remove_prop but oracle has key={}",
                                key
                            );
                        }
                        Err(e) => {
                            prop_assert!(
                                false,
                                "remove_prop unexpected error: {e:?}; key={key} field={field}"
                            );
                        }
                    }
                }

                Op::SetYear(k, v) => {
                    let key = format!("k{k}");
                    let val = year_val(*v);
                    let db_ok = db.set_prop(&key, "year", val.clone()).is_ok();
                    let or_ok = oracle.set_prop(&key, "year", val);
                    prop_assert_eq!(db_ok, or_ok);
                }

                Op::SetLoc(k, v) => {
                    let key = format!("k{k}");
                    let val = loc_val(*v);
                    let db_ok = db.set_prop(&key, "loc", val.clone()).is_ok();
                    let or_ok = oracle.set_prop(&key, "loc", val);
                    prop_assert_eq!(db_ok, or_ok);
                }

                Op::SetEmb(k, v) => {
                    let key = format!("k{k}");
                    let val = emb_val(*v);
                    let db_ok = db.set_prop(&key, "emb", val.clone()).is_ok();
                    let or_ok = oracle.set_prop(&key, "emb", val);
                    prop_assert_eq!(db_ok, or_ok);
                }

                Op::CreateView(n) => {
                    let def = view_template(*n);
                    let db_ok = db.create_view(def.clone()).is_ok();
                    let or_ok = oracle.create_view(def);
                    prop_assert_eq!(
                        db_ok,
                        or_ok,
                        "create_view result mismatch for template {}",
                        n % N_VIEW_TEMPLATES as u8
                    );
                }

                Op::DeleteView(n) => {
                    let name = VIEW_NAMES[(*n as usize) % N_VIEW_TEMPLATES];
                    let db_ok = db.delete_view(name).is_ok();
                    let or_ok = oracle.delete_view(name);
                    prop_assert_eq!(
                        db_ok,
                        or_ok,
                        "delete_view result mismatch for view {}",
                        name
                    );
                }

                Op::EnableFulltext(l, f) => {
                    let label = ft_label(*l);
                    let field = ft_field(*f);
                    let db_ok = db.enable_fulltext(label, field).is_ok();
                    let or_ok = oracle.enable_fulltext(label, field);
                    prop_assert_eq!(
                        db_ok,
                        or_ok,
                        "enable_fulltext({},{}) result mismatch",
                        label,
                        field
                    );
                }

                Op::DisableFulltext(l, f) => {
                    let label = ft_label(*l);
                    let field = ft_field(*f);
                    let db_ok = db.disable_fulltext(label, field).is_ok();
                    let or_ok = oracle.disable_fulltext(label, field);
                    prop_assert_eq!(
                        db_ok,
                        or_ok,
                        "disable_fulltext({},{}) result mismatch",
                        label,
                        field
                    );
                }

                Op::FulltextSearch(f, q) => {
                    let field = ft_field(*f);
                    let query = ft_query(*q);
                    let engine_keys: Vec<String> = db.search(field, query).into_iter().map(|(k, _)| k).collect();
                    let oracle_keys: Vec<String> = oracle.scratch_search(field, query).into_iter().map(|(k, _)| k).collect();
                    prop_assert_eq!(
                        engine_keys,
                        oracle_keys,
                        "fulltext search ordering mismatch field={} query={:?}",
                        field,
                        query
                    );
                }

                Op::Batch(sub) => {
                    // Engine: one atomic Batch frame. Oracle has no WAL — apply
                    // sequential user-level ops only when commit succeeds
                    // (engine apply is sequential too). A rejected commit
                    // leaves both states unchanged.
                    //
                    // Sequential RuleOwned/KeyNotFound on a success path is
                    // the documented T5 same-batch rule-window (validation
                    // cannot see edges a SetProp/CreateRule in this batch will
                    // derive); skip that leaf — brute-force `all_edges` still
                    // has to match after the rest of the sequence.
                    let mut b = db.batch();
                    for op in sub {
                        queue_batch_op(&mut b, op);
                    }
                    if b.commit().is_ok() {
                        for op in sub {
                            let _ = apply_oracle_leaf(&mut oracle, op);
                        }
                    }
                }
            }
        }

        // --- Full-state comparison ---

        // Live count (tombstones excluded). Engine `node_count()` is id-slot
        // count and stays high after delete; compare the T6 live stat.
        prop_assert_eq!(db.stats().nodes_live, oracle.node_count());

        // Prop sweeps: seed/year/loc/emb (InsertNode), p, f, tags, plus Set*.
        for n in 0..=255u8 {
            let key = format!("k{n}");
            for field in &["seed", "p", "f", "tags", "year", "loc", "emb"] {
                let db_prop = db.get_prop(&key, field);
                prop_assert_eq!(
                    db_prop.as_ref(),
                    oracle.get_prop(&key, field),
                    "prop mismatch key={} field={}",
                    key,
                    field
                );
            }
        }

        // View invariant: for each live view, incremental stored value ==
        // scratch recompute.  Degree/Count/Min/Max: exact equality.
        // Sum/Avg: epsilon (1e-6) to tolerate disclosed f64 accumulation drift.
        for view_def in db.views() {
            for n in 0..=255u8 {
                let key = format!("k{n}");
                if !db.has_node(&key) {
                    continue;
                }
                let stored = db.get_view_prop(&key, &view_def.view_prop);
                let scratch = db.scratch_view_value(&key, &view_def.name);
                let is_float_agg = matches!(
                    &view_def.source,
                    ViewSource::NeighborAgg {
                        agg: AggFn::Sum | AggFn::Avg,
                        ..
                    }
                );
                if is_float_agg {
                    match (stored, scratch) {
                        (None, None) => {}
                        (Some(Value::Float(s)), Some(Value::Float(sc))) => {
                            prop_assert!(
                                (s - sc).abs() < 1e-6,
                                "view {} key={}: stored {} scratch {} (epsilon exceeded)",
                                view_def.name, key, s, sc
                            );
                        }
                        (s, sc) => {
                            prop_assert!(
                                s == sc,
                                "view {} key={}: type mismatch stored {:?} scratch {:?}",
                                view_def.name, key, s, sc
                            );
                        }
                    }
                } else {
                    prop_assert_eq!(
                        stored,
                        scratch,
                        "view {} key={}: incremental vs scratch mismatch",
                        view_def.name, key
                    );
                }
            }
        }

        // Req 5: engine full edge set (user ∪ derived) == oracle.all_edges().
        // Engine edge set is built by sweeping neighbors for all 256 keys × all
        // user+rule etypes × both directions, then deduplicating
        // Out/In entries into a single (etype, src, dst) set.
        let engine_edges = sweep_engine_edges(&db);
        let oracle_edges = oracle.all_edges();
        prop_assert_eq!(
            &engine_edges,
            &oracle_edges,
            "final edge set mismatch (engine vs oracle)"
        );
        if let Err(msg) = assert_weights_match(&db, &oracle) {
            prop_assert!(false, "{msg}");
        }

        // Req 6: rebuild-is-noop invariant.
        // For every live rule, rebuild it and re-sweep; the edge set must not change.
        // Equivalence-pool rules keep `max_edges: None` — budget-trip determinism
        // is covered by T6 tests, not this suite (frozen-tripped + rebuild-exit).
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

        // Req 7: fulltext oracle equivalence — for every field in FT_FIELDS and
        // every query in FT_QUERIES, db.search ordering == oracle.scratch_search ordering.
        for field in &FT_FIELDS {
            for query in &FT_QUERIES {
                let engine_keys: Vec<String> = db.search(field, query).into_iter().map(|(k, _)| k).collect();
                let oracle_keys: Vec<String> = oracle.scratch_search(field, query).into_iter().map(|(k, _)| k).collect();
                prop_assert_eq!(
                    engine_keys,
                    oracle_keys,
                    "fulltext final-state ordering mismatch field={} query={}",
                    field,
                    query
                );
            }
        }
    }
}

/// Deterministic incremental oracle comparison for via-hop rules.
///
/// Drives: insert src/dst/via nodes, insert/delete the via-edge, mutate via
/// and dst props — asserting `engine_edges == oracle.all_edges()` at each
/// step and explicitly verifying at least one `r_via` derived edge appeared
/// (non-vacuity check).
#[test]
fn via_hop_oracle_incremental_non_vacuous() {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();
    let mut oracle = Oracle::new();

    let f_same = vec![("f".to_string(), Value::Str("shared".into()))];
    let f_diff = vec![("f".to_string(), Value::Str("other".into()))];

    // src: L0, dst: L0, via: L1 — all with f="shared" initially.
    db.insert_node("L0", "src0", f_same.clone()).unwrap();
    oracle.insert_node("L0", "src0", &f_same);
    db.insert_node("L0", "dst0", f_same.clone()).unwrap();
    oracle.insert_node("L0", "dst0", &f_same);
    db.insert_node("L1", "via0", f_same.clone()).unwrap();
    oracle.insert_node("L1", "via0", &f_same);

    // Create the r_via rule (template 9).
    let rule = rule_template(9);
    db.create_rule(rule.clone()).unwrap();
    oracle.create_rule(rule);

    // Step 1: no via-edge yet — no r_via edges.
    let eng = collect_r_via_edges(&db);
    let orc: Vec<_> = oracle
        .all_edges()
        .into_iter()
        .filter(|(et, _, _)| et == "r_via")
        .collect();
    assert_eq!(eng, orc, "step1: no e0 edge → no r_via edges");
    assert!(eng.is_empty(), "step1 must be vacuous (no via edge yet)");

    // Step 2: insert e0 src0→via0 — rule fires src0→dst0 (via0.f == dst0.f).
    db.insert_edge("e0", "src0", "via0").unwrap();
    oracle.insert_edge("e0", "src0", "via0");
    let eng = collect_r_via_edges(&db);
    let orc: Vec<_> = oracle
        .all_edges()
        .into_iter()
        .filter(|(et, _, _)| et == "r_via")
        .collect();
    assert_eq!(eng, orc, "step2: e0 inserted, r_via must fire");
    // Non-vacuity: at least one r_via edge must exist.
    assert!(
        eng.iter()
            .any(|(et, s, d)| et == "r_via" && s == "src0" && d == "dst0"),
        "non-vacuous: r_via src0→dst0 must fire when via0.f == dst0.f"
    );

    // Step 3: change via0.f to "other" — predicate no longer matches, r_via retracts.
    db.set_prop("via0", "f", Value::Str("other".into()))
        .unwrap();
    oracle.set_prop("via0", "f", Value::Str("other".into()));
    let eng = collect_r_via_edges(&db);
    let orc: Vec<_> = oracle
        .all_edges()
        .into_iter()
        .filter(|(et, _, _)| et == "r_via")
        .collect();
    assert_eq!(eng, orc, "step3: via prop changed, r_via must retract");
    assert!(eng.is_empty(), "step3: no r_via after via.f mismatch");

    // Step 4: restore via0.f to match — r_via fires again.
    db.set_prop("via0", "f", Value::Str("shared".into()))
        .unwrap();
    oracle.set_prop("via0", "f", Value::Str("shared".into()));
    let eng = collect_r_via_edges(&db);
    let orc: Vec<_> = oracle
        .all_edges()
        .into_iter()
        .filter(|(et, _, _)| et == "r_via")
        .collect();
    assert_eq!(eng, orc, "step4: via prop restored, r_via fires again");
    assert!(
        eng.iter()
            .any(|(et, s, d)| et == "r_via" && s == "src0" && d == "dst0"),
        "step4: r_via must re-fire after via.f restored"
    );

    // Step 5: delete e0 src0→via0 — r_via retracts.
    db.delete_edge("e0", "src0", "via0").unwrap();
    oracle.delete_edge("e0", "src0", "via0");
    let eng = collect_r_via_edges(&db);
    let orc: Vec<_> = oracle
        .all_edges()
        .into_iter()
        .filter(|(et, _, _)| et == "r_via")
        .collect();
    assert_eq!(eng, orc, "step5: via edge deleted, r_via retracts");
    assert!(eng.is_empty(), "step5: no r_via after via edge deleted");

    // Step 6: self-edge guard — add src0 as its own via dst (if src_label == dst_label).
    // r_via has src_label == dst_label == "L0". Ensure src0→src0 never appears.
    db.insert_edge("e0", "src0", "via0").unwrap();
    oracle.insert_edge("e0", "src0", "via0");
    let eng = collect_r_via_edges(&db);
    assert!(
        !eng.iter().any(|(_, s, d)| s == d),
        "step6: no self-edges in r_via output"
    );
    let orc: Vec<_> = oracle
        .all_edges()
        .into_iter()
        .filter(|(et, _, _)| et == "r_via")
        .collect();
    assert_eq!(eng, orc, "step6: engine == oracle after via re-insert");

    // Add dst0 also as src (same f) — ensure src0→src0 is still absent.
    // The "dst" of src0 is dst0; src0 should NOT appear as its own destination.
    let _ = f_diff; // f_diff declared but only used to show contrast
}

/// Collect all r_via (etype == "r_via") edges from the engine as sorted triples.
fn collect_r_via_edges(db: &GraphDb<SimFs>) -> Vec<(String, String, String)> {
    let mut out = BTreeSet::new();
    for n in 0..=255u8 {
        let key = format!("k{n}");
        for dir in [Direction::Out, Direction::In] {
            for nb in db.neighbors(&key, "r_via", dir).unwrap_or_default() {
                let (s, d) = match dir {
                    Direction::Out => (key.clone(), nb),
                    Direction::In => (nb, key.clone()),
                };
                out.insert(("r_via".to_string(), s, d));
            }
        }
    }
    // Also sweep named nodes used by the non-vacuity test.
    for key in ["src0", "dst0", "via0"] {
        for dir in [Direction::Out, Direction::In] {
            for nb in db.neighbors(key, "r_via", dir).unwrap_or_default() {
                let (s, d) = match dir {
                    Direction::Out => (key.to_string(), nb),
                    Direction::In => (nb, key.to_string()),
                };
                out.insert(("r_via".to_string(), s, d));
            }
        }
    }
    out.into_iter().collect()
}

/// User-first edge on a pair a later rule would derive: `delete_edge` must
/// be `RuleOwned` (the rule would just put it back). A hole here is an
/// engine/oracle edge-set divergence and a rebuild-is-noop failure.
#[test]
fn delete_edge_of_user_first_derived_pair_is_rule_owned() {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();
    let mut oracle = Oracle::new();
    let props = vec![("f".to_string(), Value::Str("same".into()))];
    assert!(db.insert_node("L0", "k1", props.clone()).is_ok());
    assert!(oracle.insert_node("L0", "k1", &props));
    assert!(db.insert_node("L0", "k2", props.clone()).is_ok());
    assert!(oracle.insert_node("L0", "k2", &props));
    assert!(db.insert_edge("r_fe", "k1", "k2").unwrap());
    assert_eq!(oracle.insert_edge("r_fe", "k1", "k2"), Some(true));
    let def = rule_template(1); // FieldEqual on "f", etype r_fe
    assert!(db.create_rule(def.clone()).is_ok());
    assert!(oracle.create_rule(def));
    assert!(oracle.is_derived_edge("r_fe", "k1", "k2"));

    match db.delete_edge("r_fe", "k1", "k2") {
        Err(GraphError::RuleOwned { detail }) => {
            assert!(
                detail.contains("or a live rule would re-derive it"),
                "would_derive RuleOwned must distinguish from provenance: {detail}"
            );
        }
        other => panic!("expected RuleOwned, got {other:?}"),
    }
    assert_eq!(oracle.delete_edge("r_fe", "k1", "k2"), Some(None));
    let engine_edges = sweep_engine_edges(&db);
    assert_eq!(engine_edges, oracle.all_edges());
    db.rebuild_rule("r_fe").unwrap();
    assert_eq!(sweep_engine_edges(&db), engine_edges);
}

// ---------------------------------------------------------------------------
// Approximate-rule tests (IVF-Flat)
// ---------------------------------------------------------------------------

// Recall floors imported from sim_harness (canonical location: crates/sim-harness/src/lib.rs).

fn recall(
    approx: &BTreeSet<(String, String, String)>,
    exact: &BTreeSet<(String, String, String)>,
) -> f64 {
    if exact.is_empty() {
        return 1.0;
    }
    let hits = exact.iter().filter(|e| approx.contains(e)).count();
    hits as f64 / exact.len() as f64
}

/// WAL replay identity: build an approximate rule, replay the WAL, compare
/// the full derived edge set. Same rule + same data → same clusters → same edges.
#[test]
fn approximate_wal_replay_identity() {
    let dir = {
        let d = std::env::temp_dir().join(format!("graphdb-approx-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    };

    // Build initial state: 20 2-D unit vectors in 4 clusters.
    let vecs: &[[f64; 2]] = &[
        [1.0, 0.0],
        [0.98, 0.2],
        [0.96, 0.28],
        [0.97, 0.24],
        [0.0, 1.0],
        [0.1, 0.995],
        [0.05, 0.999],
        [0.08, 0.997],
        [-1.0, 0.0],
        [-0.98, 0.2],
        [-0.96, -0.28],
        [-0.97, 0.24],
        [0.0, -1.0],
        [0.1, -0.995],
        [-0.05, -0.999],
        [-0.08, -0.997],
        [0.7, 0.714],
        [0.71, 0.704],
        [-0.7, 0.714],
        [-0.71, -0.704],
    ];

    let mut db = GraphDb::open(&dir).unwrap();
    for (i, v) in vecs.iter().enumerate() {
        let key = format!("v{i}");
        let norm = (v[0] * v[0] + v[1] * v[1]).sqrt();
        let val = Value::List(vec![Value::Float(v[0] / norm), Value::Float(v[1] / norm)]);
        db.insert_node("V", &key, vec![("emb".into(), val)])
            .unwrap();
    }

    db.create_rule(RuleDef {
        name: "approx_sim".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        },
        edge_type: "ASIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();

    // Capture edge set.
    let n = vecs.len();
    let mut edges_original = BTreeSet::new();
    for i in 0..n {
        let src = format!("v{i}");
        for nb in db
            .neighbors(&src, "ASIM", Direction::Out)
            .unwrap_or_default()
        {
            edges_original.insert(("ASIM".to_string(), src.clone(), nb));
        }
    }
    drop(db);

    // Reopen (WAL replay).
    let db2 = GraphDb::open(&dir).unwrap();
    let mut edges_replayed = BTreeSet::new();
    for i in 0..n {
        let src = format!("v{i}");
        for nb in db2
            .neighbors(&src, "ASIM", Direction::Out)
            .unwrap_or_default()
        {
            edges_replayed.insert(("ASIM".to_string(), src.clone(), nb));
        }
    }

    assert_eq!(
        edges_original, edges_replayed,
        "WAL replay must produce identical derived set for approximate rule"
    );
}

/// Recall of approximate rule on a quiesced graph (all nodes present, rebuild done).
/// Asserts recall ≥ APPROX_RECALL_FLOOR_QUIESCED = 0.90.
#[test]
fn approximate_recall_above_floor_quiesced() {
    // 20 2-D unit vectors matching the WAL replay test setup.
    let vecs: &[[f64; 2]] = &[
        [1.0, 0.0],
        [0.98, 0.2],
        [0.96, 0.28],
        [0.97, 0.24],
        [0.0, 1.0],
        [0.1, 0.995],
        [0.05, 0.999],
        [0.08, 0.997],
        [-1.0, 0.0],
        [-0.98, 0.2],
        [-0.96, -0.28],
        [-0.97, 0.24],
        [0.0, -1.0],
        [0.1, -0.995],
        [-0.05, -0.999],
        [-0.08, -0.997],
        [0.7, 0.714],
        [0.71, 0.704],
        [-0.7, 0.714],
        [-0.71, -0.704],
    ];
    let min_sim = 0.9_f64;

    let dir = {
        let d =
            std::env::temp_dir().join(format!("graphdb-approx-recall-q-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    };

    let mut db = GraphDb::open(&dir).unwrap();
    let mut normalized: Vec<(String, Vec<f64>)> = Vec::new();
    for (i, v) in vecs.iter().enumerate() {
        let key = format!("v{i}");
        let norm = (v[0] * v[0] + v[1] * v[1]).sqrt();
        let nv = vec![v[0] / norm, v[1] / norm];
        let val = Value::List(vec![Value::Float(nv[0]), Value::Float(nv[1])]);
        db.insert_node("V", &key, vec![("emb".into(), val)])
            .unwrap();
        normalized.push((format!("V{key}"), nv));
    }

    db.create_rule(RuleDef {
        name: "approx_sim".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: min_sim,
        },
        edge_type: "ASIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();

    // Compute approximate edge set from engine.
    let approx_edges: BTreeSet<(String, String, String)> = {
        let mut s = BTreeSet::new();
        for i in 0..vecs.len() {
            let src = format!("v{i}");
            for nb in db
                .neighbors(&src, "ASIM", Direction::Out)
                .unwrap_or_default()
            {
                s.insert(("ASIM".to_string(), src.clone(), nb));
            }
        }
        s
    };

    // Compute exact ground-truth.
    let exact_edges: BTreeSet<(String, String, String)> = {
        let mut s = BTreeSet::new();
        for (i, vi) in vecs.iter().enumerate() {
            let norm_i = (vi[0] * vi[0] + vi[1] * vi[1]).sqrt();
            let ni = [vi[0] / norm_i, vi[1] / norm_i];
            for (j, vj) in vecs.iter().enumerate() {
                if i == j {
                    continue;
                }
                let norm_j = (vj[0] * vj[0] + vj[1] * vj[1]).sqrt();
                let nj = [vj[0] / norm_j, vj[1] / norm_j];
                let dot = ni[0] * nj[0] + ni[1] * nj[1];
                if dot >= min_sim {
                    s.insert(("ASIM".to_string(), format!("v{i}"), format!("v{j}")));
                }
            }
        }
        s
    };

    let r = recall(&approx_edges, &exact_edges);
    assert!(
        r >= APPROX_RECALL_FLOOR_QUIESCED,
        "quiesced recall {:.3} < floor {:.3} (approx={} exact={})",
        r,
        APPROX_RECALL_FLOOR_QUIESCED,
        approx_edges.len(),
        exact_edges.len()
    );
}

/// Recall after rebuild (simulates crash-recovery re-fit path).
/// Asserts recall ≥ APPROX_RECALL_FLOOR_RECOVERY = 0.85.
#[test]
fn approximate_recall_above_floor_after_rebuild() {
    let vecs: &[[f64; 2]] = &[
        [1.0, 0.0],
        [0.98, 0.2],
        [0.96, 0.28],
        [0.97, 0.24],
        [0.0, 1.0],
        [0.1, 0.995],
        [0.05, 0.999],
        [0.08, 0.997],
        [-1.0, 0.0],
        [-0.98, 0.2],
        [-0.96, -0.28],
        [-0.97, 0.24],
        [0.0, -1.0],
        [0.1, -0.995],
        [-0.05, -0.999],
        [-0.08, -0.997],
        [0.7, 0.714],
        [0.71, 0.704],
        [-0.7, 0.714],
        [-0.71, -0.704],
    ];
    let min_sim = 0.9_f64;

    let dir = {
        let d =
            std::env::temp_dir().join(format!("graphdb-approx-recall-r-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    };

    let mut db = GraphDb::open(&dir).unwrap();
    for (i, v) in vecs.iter().enumerate() {
        let key = format!("v{i}");
        let norm = (v[0] * v[0] + v[1] * v[1]).sqrt();
        let val = Value::List(vec![Value::Float(v[0] / norm), Value::Float(v[1] / norm)]);
        db.insert_node("V", &key, vec![("emb".into(), val)])
            .unwrap();
    }

    db.create_rule(RuleDef {
        name: "approx_sim".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: min_sim,
        },
        edge_type: "ASIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();

    // Trigger rebuild (simulates crash-recovery re-fit).
    db.rebuild_rule("approx_sim").unwrap();

    // Compute approximate edge set after rebuild.
    let approx_edges: BTreeSet<(String, String, String)> = {
        let mut s = BTreeSet::new();
        for i in 0..vecs.len() {
            let src = format!("v{i}");
            for nb in db
                .neighbors(&src, "ASIM", Direction::Out)
                .unwrap_or_default()
            {
                s.insert(("ASIM".to_string(), src.clone(), nb));
            }
        }
        s
    };

    // Compute exact ground-truth.
    let exact_edges: BTreeSet<(String, String, String)> = {
        let mut s = BTreeSet::new();
        for (i, vi) in vecs.iter().enumerate() {
            let norm_i = (vi[0] * vi[0] + vi[1] * vi[1]).sqrt();
            let ni = [vi[0] / norm_i, vi[1] / norm_i];
            for (j, vj) in vecs.iter().enumerate() {
                if i == j {
                    continue;
                }
                let norm_j = (vj[0] * vj[0] + vj[1] * vj[1]).sqrt();
                let nj = [vj[0] / norm_j, vj[1] / norm_j];
                let dot = ni[0] * nj[0] + ni[1] * nj[1];
                if dot >= min_sim {
                    s.insert(("ASIM".to_string(), format!("v{i}"), format!("v{j}")));
                }
            }
        }
        s
    };

    let r = recall(&approx_edges, &exact_edges);
    assert!(
        r >= APPROX_RECALL_FLOOR_RECOVERY,
        "rebuild recall {:.3} < floor {:.3} (approx={} exact={})",
        r,
        APPROX_RECALL_FLOOR_RECOVERY,
        approx_edges.len(),
        exact_edges.len()
    );
}

// ---------------------------------------------------------------------------
// High-dimensional recall test (1536-D, 1024 clustered vectors)
// ---------------------------------------------------------------------------

/// Seeded LCG — same formula used in the engine bench `mix()`.
fn lcg_mix(seed: u64, i: u64, j: u64) -> u64 {
    let mut x = seed
        .wrapping_add(i.wrapping_mul(6364136223846793005))
        .wrapping_add(j.wrapping_mul(1442695040888963407));
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^= x >> 33;
    x
}

/// Build a unit-normed 1536-D vector for cluster `c` node `n` with given seed.
/// Strategy: start from a cluster centroid direction (dims 0..1535 where
/// dim 0 has a large bias for cluster c), then perturb with noise, then normalise.
fn clustered_vec_1536(seed: u64, n_clusters: usize, cluster: usize, member: usize) -> Vec<f64> {
    const DIM: usize = 1536;
    let mut v = vec![0.0f64; DIM];
    // Cluster centroid: set the cluster's "primary" dimension to a large positive value.
    // With 1536 dims and n_clusters ≤ 32, each cluster owns 48+ dims.
    let stride = DIM / n_clusters;
    let base = cluster * stride;
    for x in v[base..(base + stride).min(DIM)].iter_mut() {
        *x = 1.0;
    }
    // Add small seeded noise to each dim.
    for (d, x) in v.iter_mut().enumerate() {
        let bits = lcg_mix(seed, cluster as u64 * 10_000 + member as u64, d as u64);
        let noise = (bits as f64) / (u64::MAX as f64) * 0.15 - 0.075; // ±7.5% noise
        *x += noise;
    }
    // Normalise.
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt().max(1e-12);
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

/// Recall at real scale: 1024 nodes, 1536-D, 16 clusters × 64 nodes each.
/// min_sim = 0.80 (within-cluster cosine is ~0.90+ for this generation scheme).
/// Asserts recall ≥ APPROX_RECALL_FLOOR_QUIESCED = 0.90 (quiesced).
///
/// Marked `#[ignore]` because the O(n²) exact ground-truth over 1024×1536 dims
/// is ~70s in debug. Run with `cargo test -- --include-ignored` for full DST.
#[test]
#[ignore]
fn approximate_recall_above_floor_1536dim_1k() {
    const N_CLUSTERS: usize = 16;
    const PER_CLUSTER: usize = 64;
    const N: usize = N_CLUSTERS * PER_CLUSTER; // 1024
    const MIN_SIM: f64 = 0.80;
    const SEED: u64 = 0xdead_beef_cafe_babe;

    let dir = {
        let d = std::env::temp_dir().join(format!("graphdb-approx-1536-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    };

    // Build node vectors.
    let vecs: Vec<Vec<f64>> = (0..N_CLUSTERS)
        .flat_map(|c| (0..PER_CLUSTER).map(move |m| (c, m)))
        .map(|(c, m)| clustered_vec_1536(SEED, N_CLUSTERS, c, m))
        .collect();

    let mut db = GraphDb::open(&dir).unwrap();
    for (i, v) in vecs.iter().enumerate() {
        let key = format!("v{i:04}");
        let val = Value::List(v.iter().copied().map(Value::Float).collect());
        db.insert_node("V", &key, vec![("emb".into(), val)])
            .unwrap();
    }

    db.create_rule(RuleDef {
        name: "approx_sim".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: MIN_SIM,
        },
        edge_type: "ASIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();

    // Collect approximate edge set.
    let approx_edges: BTreeSet<(String, String, String)> = {
        let mut s = BTreeSet::new();
        for i in 0..N {
            let src = format!("v{i:04}");
            for nb in db
                .neighbors(&src, "ASIM", Direction::Out)
                .unwrap_or_default()
            {
                s.insert(("ASIM".to_string(), src.clone(), nb));
            }
        }
        s
    };

    // Compute exact ground-truth (O(n²) — acceptable for n=1024 in tests).
    let exact_edges: BTreeSet<(String, String, String)> = {
        let mut s = BTreeSet::new();
        for i in 0..N {
            for j in 0..N {
                if i == j {
                    continue;
                }
                let dot: f64 = vecs[i].iter().zip(vecs[j].iter()).map(|(a, b)| a * b).sum();
                if dot >= MIN_SIM {
                    s.insert(("ASIM".to_string(), format!("v{i:04}"), format!("v{j:04}")));
                }
            }
        }
        s
    };

    let r = recall(&approx_edges, &exact_edges);
    assert!(
        r >= APPROX_RECALL_FLOOR_QUIESCED,
        "1536-D 1k quiesced recall {:.3} < floor {:.3} (approx={} exact={})",
        r,
        APPROX_RECALL_FLOOR_QUIESCED,
        approx_edges.len(),
        exact_edges.len()
    );
}

/// 5k-node IVF-Flat wall-clock probe with recall vs exact ground truth.
///
/// Shape: 5000 nodes × 1536-D, min_sim=0.85, approximate=true.
/// Records: IVF backfill wall-clock, recall vs brute-force exact.
/// `#[ignore]` — exact O(n²) ground truth is expensive; run explicitly with
/// `cargo test --release -- --ignored approximate_recall_5k_timing --nocapture`.
#[test]
#[ignore]
fn approximate_recall_5k_timing() {
    use std::time::Instant;

    const N_CLUSTERS: usize = 50;
    const PER_CLUSTER: usize = 100;
    const N: usize = N_CLUSTERS * PER_CLUSTER; // 5000
    const MIN_SIM: f64 = 0.85;
    const SEED: u64 = 0xcafe_f00d_dead_1234;

    let dir = {
        let d = std::env::temp_dir().join(format!("graphdb-approx-5k-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    };

    // Build clustered 1536-D node vectors (same generator as 1k test).
    let vecs: Vec<Vec<f64>> = (0..N_CLUSTERS)
        .flat_map(|c| (0..PER_CLUSTER).map(move |m| (c, m)))
        .map(|(c, m)| clustered_vec_1536(SEED, N_CLUSTERS, c, m))
        .collect();

    // Insert nodes (not timed — pure data setup).
    let mut db = GraphDb::open(&dir).unwrap();
    for (i, v) in vecs.iter().enumerate() {
        let key = format!("v{i:04}");
        let val = Value::List(v.iter().copied().map(Value::Float).collect());
        db.insert_node("V", &key, vec![("emb".into(), val)])
            .unwrap();
    }

    // Time the IVF-Flat backfill (k-means fit + candidate probing + edge derivation).
    let t0 = Instant::now();
    db.create_rule(RuleDef {
        name: "approx_sim5k".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: MIN_SIM,
        },
        edge_type: "ASIM5K".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    let ivf_ms = t0.elapsed().as_millis();

    // Collect approximate edges.
    let approx_edges: BTreeSet<(String, String, String)> = {
        let mut s = BTreeSet::new();
        for i in 0..N {
            let src = format!("v{i:04}");
            for nb in db
                .neighbors(&src, "ASIM5K", Direction::Out)
                .unwrap_or_default()
            {
                s.insert(("ASIM5K".to_string(), src.clone(), nb));
            }
        }
        s
    };

    // Exact O(n²) ground truth (slow — run only in release mode).
    let t1 = Instant::now();
    let exact_edges: BTreeSet<(String, String, String)> = {
        let mut s = BTreeSet::new();
        for i in 0..N {
            for j in 0..N {
                if i == j {
                    continue;
                }
                let dot: f64 = vecs[i].iter().zip(vecs[j].iter()).map(|(a, b)| a * b).sum();
                if dot >= MIN_SIM {
                    s.insert(("ASIM5K".to_string(), format!("v{i:04}"), format!("v{j:04}")));
                }
            }
        }
        s
    };
    let exact_ms = t1.elapsed().as_millis();

    let r = recall(&approx_edges, &exact_edges);
    eprintln!(
        "5k probe: IVF backfill {ivf_ms}ms | exact ground truth {exact_ms}ms | \
         recall {r:.4} (approx={} exact={})",
        approx_edges.len(),
        exact_edges.len()
    );
    assert!(
        r >= APPROX_RECALL_FLOOR_QUIESCED,
        "5k recall {:.3} < floor {:.3} (approx={} exact={})",
        r,
        APPROX_RECALL_FLOOR_QUIESCED,
        approx_edges.len(),
        exact_edges.len()
    );
}

/// IVF cleanup on delete under approximate=true rule.
///
/// Setup: 20 2-D unit vectors in 4 clusters; approximate VectorSimilar rule with
/// IVF genuinely active (n=20 > IVF_K_MIN=4, so scan-all fallback does not fire).
///
/// After inserting all nodes and creating the rule (nodes first, rule second so IVF
/// fits during backfill — same pattern as `approximate_recall_above_floor_quiesced`):
///   (i)  Delete one node from cluster 0 (v0).
///   (ii) Assert v0 never appears as a derived-edge dst or src afterward.
///   (iii) Assert the post-delete derived set matches a reference built without v0.
///   (iv) Assert the IVF dst-side drift counter > 0 (removal incremented it).
#[test]
fn ivf_cleanup_on_delete_under_approximate_rule() {
    // 20 2-D unit vectors matching the existing approximate-rule tests.
    // Cluster 0: v0..v3 (near [1,0]), Cluster 1: v4..v7 (near [0,1]),
    // Cluster 2: v8..v11 (near [-1,0]), Cluster 3: v12..v15 (near [0,-1]),
    // Extras: v16..v19 (diagonals).
    let vecs: &[[f64; 2]] = &[
        [1.0, 0.0], // v0 — will be deleted
        [0.98, 0.2],
        [0.96, 0.28],
        [0.97, 0.24],
        [0.0, 1.0],
        [0.1, 0.995],
        [0.05, 0.999],
        [0.08, 0.997],
        [-1.0, 0.0],
        [-0.98, 0.2],
        [-0.96, -0.28],
        [-0.97, 0.24],
        [0.0, -1.0],
        [0.1, -0.995],
        [-0.05, -0.999],
        [-0.08, -0.997],
        [0.7, 0.714],
        [0.71, 0.704],
        [-0.7, 0.714],
        [-0.71, -0.704],
    ];
    let min_sim = 0.9_f64;
    let n = vecs.len();
    let deleted_key = "v0";

    let dir = {
        let d = std::env::temp_dir().join(format!("graphdb-ivf-delete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    };

    // Nodes first, then rule (IVF fits at backfill time with all n nodes).
    let mut db = GraphDb::open(&dir).unwrap();
    let mut normalized: Vec<(String, [f64; 2])> = Vec::new();
    for (i, v) in vecs.iter().enumerate() {
        let key = format!("v{i}");
        let norm = (v[0] * v[0] + v[1] * v[1]).sqrt();
        let nv = [v[0] / norm, v[1] / norm];
        let val = Value::List(vec![Value::Float(nv[0]), Value::Float(nv[1])]);
        db.insert_node("V", &key, vec![("emb".into(), val)])
            .unwrap();
        normalized.push((key, nv));
    }

    db.create_rule(RuleDef {
        name: "ivf_sim".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: min_sim,
        },
        edge_type: "IVSIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();

    // Drift should be 0 before any deletion (IVF just fitted).
    let drift_before = db
        .ivf_dst_drift("ivf_sim")
        .expect("rule must be approximate");
    assert_eq!(drift_before, 0, "drift must be 0 immediately after IVF fit");

    // Delete v0.
    db.delete_node(deleted_key).unwrap();

    // (i) v0 must never appear as a derived-edge source or destination.
    for i in 0..n {
        let src = format!("v{i}");
        if src == deleted_key {
            continue;
        }
        let dsts = db
            .neighbors(&src, "IVSIM", Direction::Out)
            .unwrap_or_default();
        assert!(
            !dsts.contains(&deleted_key.to_string()),
            "v0 (deleted) must not appear as dst of {src} after deletion; got {dsts:?}"
        );
    }
    let src_edges_of_v0 = db
        .neighbors(deleted_key, "IVSIM", Direction::Out)
        .unwrap_or_default();
    assert!(
        src_edges_of_v0.is_empty(),
        "v0 (deleted) must not appear as src; got {src_edges_of_v0:?}"
    );

    // (ii) Post-delete derived set matches a reference built without v0.
    // Reference: exact pairs among remaining nodes with cosine ≥ min_sim.
    let reference_edges: BTreeSet<(String, String)> = {
        let mut s = BTreeSet::new();
        for (i, (ki, ni)) in normalized.iter().enumerate() {
            if ki == deleted_key {
                continue;
            }
            for (j, (kj, nj)) in normalized.iter().enumerate() {
                if i == j || kj == deleted_key {
                    continue;
                }
                let dot = ni[0] * nj[0] + ni[1] * nj[1];
                if dot >= min_sim {
                    s.insert((ki.clone(), kj.clone()));
                }
            }
        }
        s
    };
    let engine_edges: BTreeSet<(String, String)> = {
        let mut s = BTreeSet::new();
        for i in 0..n {
            let src = format!("v{i}");
            if src == deleted_key || !db.has_node(&src) {
                continue;
            }
            for dst in db
                .neighbors(&src, "IVSIM", Direction::Out)
                .unwrap_or_default()
            {
                s.insert((src.clone(), dst));
            }
        }
        s
    };
    // Recall check: engine edges must be a high-recall subset of exact edges.
    // (Approximate mode: perfect recall not guaranteed, but floor applies.)
    let exact_count = reference_edges.len();
    if exact_count > 0 {
        let hit_count = reference_edges
            .iter()
            .filter(|e| engine_edges.contains(*e))
            .count();
        let r = hit_count as f64 / exact_count as f64;
        // After deletion the cluster centroids are not re-fitted (drift > 0 but
        // no rebuild yet), so recall may fall to the recovery floor.
        assert!(
            r >= APPROX_RECALL_FLOOR_RECOVERY,
            "post-delete recall {r:.3} < floor {:.3} (exact={exact_count} hits={hit_count})",
            APPROX_RECALL_FLOOR_RECOVERY
        );
    }
    // No false positives from the deleted node.
    for (src, dst) in &engine_edges {
        assert_ne!(src, deleted_key, "deleted v0 must not be an edge src");
        assert_ne!(dst, deleted_key, "deleted v0 must not be an edge dst");
    }

    // (iii) IVF dst-side drift counter incremented by the deletion.
    let drift_after = db
        .ivf_dst_drift("ivf_sim")
        .expect("rule must be approximate");
    assert!(
        drift_after > 0,
        "drift counter must be > 0 after deleting a node from an IVF-indexed rule; got {drift_after}"
    );
}
