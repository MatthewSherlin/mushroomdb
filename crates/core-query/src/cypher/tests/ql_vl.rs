//! QL-VL (defect-ledger row 132): a variable-length hop applies its
//! destination's label and inline property map, in every hop form, in the
//! middle of a pattern, under OPTIONAL MATCH, with a named path or a
//! relationship-list variable, and with parameters in the map.
//!
//! Released 0.9.x returned every reachable node for
//! `(a)-[*1..3]->(b:Person {active: true})`. The randomized test at the bottom
//! compares the engine against a brute-force walk written here.

use super::tests::{run, Fx};
use crate::cypher::{lex, parse};
use core_storage::property_index::PropertyIndex;
use core_storage::Value;
use std::collections::{BTreeMap, BTreeSet};

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn b(v: bool) -> Value {
    Value::Bool(v)
}

fn rows_with(fx: &Fx, q: &str, params: &BTreeMap<String, Value>) -> Vec<Vec<Option<Value>>> {
    let rs = run(&fx.view(), q, params).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

fn rows(fx: &Fx, q: &str) -> Vec<Vec<Option<Value>>> {
    rows_with(fx, q, &BTreeMap::new())
}

/// One column of keys, as plain strings (`None` for a null cell).
fn keys(fx: &Fx, q: &str) -> Vec<Option<String>> {
    rows(fx, q)
        .into_iter()
        .map(|row| match &row[0] {
            Some(Value::Str(k)) => Some(k.clone()),
            None => None,
            other => panic!("`{q}`: not a key: {other:?}"),
        })
        .collect()
}

fn some(xs: &[&str]) -> Vec<Option<String>> {
    xs.iter().map(|x| Some((*x).to_owned())).collect()
}

/// ```text
/// s -KNOWS-> p1 (Person, active)        p1 -LIVES-> c1 (City)
/// s -KNOWS-> p2 (Person, inactive)      p2 -LIVES-> c2 (City)
/// s -OWNS->  r1 (Robot, active)         r1 -LIVES-> cr (City)
/// r1 -KNOWS-> p3 (Person, active)       p3 -LIVES-> c3 (City)
/// p2 -KNOWS-> p4 (Person, no `active`)
/// p3 -KNOWS-> p5 (Person, active)
/// p5 -KNOWS-> p6 (Person, active)       four hops from s
/// x  -KNOWS-> s  (x: Person, active)
/// ```
/// Unfiltered reachability from `s` within three hops includes cities, a
/// robot, an inactive person and a person without the property.
fn fixture() -> Fx {
    let mut fx = Fx::new();
    let st = fx.add("Person", "s", vec![("active", b(true))]);
    let p1 = fx.add("Person", "p1", vec![("active", b(true))]);
    let p2 = fx.add("Person", "p2", vec![("active", b(false))]);
    let r1 = fx.add("Robot", "r1", vec![("active", b(true))]);
    let p3 = fx.add("Person", "p3", vec![("active", b(true))]);
    let p4 = fx.add("Person", "p4", vec![]);
    let p5 = fx.add("Person", "p5", vec![("active", b(true))]);
    let p6 = fx.add("Person", "p6", vec![("active", b(true))]);
    let x = fx.add("Person", "x", vec![("active", b(true))]);
    let c1 = fx.add("City", "c1", vec![]);
    let c2 = fx.add("City", "c2", vec![]);
    let cr = fx.add("City", "cr", vec![]);
    let c3 = fx.add("City", "c3", vec![]);
    fx.edge("KNOWS", st, p1, vec![]);
    fx.edge("KNOWS", st, p2, vec![]);
    fx.edge("OWNS", st, r1, vec![]);
    fx.edge("KNOWS", r1, p3, vec![]);
    fx.edge("KNOWS", p2, p4, vec![]);
    fx.edge("KNOWS", p3, p5, vec![]);
    fx.edge("KNOWS", p5, p6, vec![]);
    fx.edge("KNOWS", x, st, vec![]);
    fx.edge("LIVES", p1, c1, vec![]);
    fx.edge("LIVES", p2, c2, vec![]);
    fx.edge("LIVES", r1, cr, vec![]);
    fx.edge("LIVES", p3, c3, vec![]);
    fx
}

#[test]
fn the_destination_label_and_properties_filter_a_variable_length_hop() {
    let fx = fixture();
    let q = "MATCH (a {id: 's'})-[*1..3]->(b:Person {active: true}) RETURN b ORDER BY b";
    assert_eq!(keys(&fx, q), some(&["p1", "p3", "p5"]));
    // Label alone, properties alone.
    let q = "MATCH (a {id: 's'})-[*1..3]->(b:Person) RETURN b ORDER BY b";
    assert_eq!(keys(&fx, q), some(&["p1", "p2", "p3", "p4", "p5"]));
    let q = "MATCH (a {id: 's'})-[*1..3]->(b {active: true}) RETURN b ORDER BY b";
    assert_eq!(keys(&fx, q), some(&["p1", "p3", "p5", "r1"]));
    // The unfiltered hop still reaches everything, cities included.
    let q = "MATCH (a {id: 's'})-[*1..3]->(b) RETURN b ORDER BY b";
    assert_eq!(
        keys(&fx, q),
        some(&["c1", "c2", "c3", "cr", "p1", "p2", "p3", "p4", "p5", "r1"])
    );
}

#[test]
fn intermediate_nodes_are_not_filtered_only_the_end_node() {
    let fx = fixture();
    // p3 is reached through the robot r1; the robot fails `:Person` but is
    // only an intermediate node, so p3 is still returned.
    let q = "MATCH (a {id: 's'})-[*2]->(b:Person {active: true}) RETURN b";
    assert_eq!(keys(&fx, q), some(&["p3"]));
}

#[test]
fn every_hop_form_filters_the_destination() {
    let fx = fixture();
    let cases: [(&str, &[&str]); 8] = [
        ("*", &["p1", "p3", "p5", "p6"]),
        ("*2", &["p3"]),
        ("*1..3", &["p1", "p3", "p5"]),
        ("*..2", &["p1", "p3"]),
        ("*2..4", &["p3", "p5", "p6"]),
        (":KNOWS*", &["p1"]),
        (":KNOWS|OWNS*1..3", &["p1", "p3", "p5"]),
        (":OWNS|LIVES*1..2", &[]),
    ];
    for (hop, want) in cases {
        let q = format!(
            "MATCH (a {{id: 's'}})-[{hop}]->(b:Person {{active: true}}) RETURN b ORDER BY b"
        );
        assert_eq!(keys(&fx, &q), some(want), "{q}");
    }
}

#[test]
fn an_open_ended_minimum_is_still_refused() {
    // `*n..` has no upper bound and stays a named error (capped at 10 hops).
    let tokens = lex("MATCH (a {id: 's'})-[*2..]->(b:Person) RETURN b").expect("lex");
    let err = parse(&tokens).expect_err("unbounded hop range");
    assert!(err.contains("capped at 10 hops"), "{err}");
}

#[test]
fn incoming_and_undirected_hops_filter_the_destination() {
    let fx = fixture();
    let q = "MATCH (a {id: 'p3'})<-[*1..3]-(b:Person {active: true}) RETURN b ORDER BY b";
    assert_eq!(keys(&fx, q), some(&["s", "x"]));
    let q = "MATCH (a {id: 'p3'})<-[*1..3]-(b:Robot) RETURN b";
    assert_eq!(keys(&fx, q), some(&["r1"]));
    let q = "MATCH (a {id: 'p1'})-[*1..2]-(b:Person {active: true}) RETURN DISTINCT b ORDER BY b";
    assert_eq!(keys(&fx, q), some(&["s", "x"]));
    let q = "MATCH (a {id: 'p1'})-[*1..2]-(b:City) RETURN b";
    assert_eq!(keys(&fx, q), some(&["c1"]));
}

#[test]
fn a_hop_in_the_middle_of_a_pattern_filters_its_destination() {
    let fx = fixture();
    let q = "MATCH (a {id: 's'})-[*1..2]->(b:Person {active: true})-[:LIVES]->(c) \
             RETURN b, c ORDER BY b";
    assert_eq!(
        rows(&fx, q),
        vec![
            vec![Some(s("p1")), Some(s("c1"))],
            vec![Some(s("p3")), Some(s("c3"))]
        ]
    );
    // Two variable-length hops in a row, each with its own filter.
    let q = "MATCH (a {id: 's'})-[*1..2]->(m:Robot)-[*1..2]->(b:Person {active: true}) \
             RETURN m, b ORDER BY b";
    assert_eq!(
        rows(&fx, q),
        vec![
            vec![Some(s("r1")), Some(s("p3"))],
            vec![Some(s("r1")), Some(s("p5"))]
        ]
    );
}

#[test]
fn optional_match_filters_the_destination() {
    let fx = fixture();
    let q = "MATCH (a {id: 's'}) OPTIONAL MATCH (a)-[*1..3]->(b:Person {active: true}) \
             RETURN b ORDER BY b";
    assert_eq!(keys(&fx, q), some(&["p1", "p3", "p5"]));
    // Nothing reachable passes the filter: one row, `b` null.
    let q = "MATCH (a {id: 's'}) OPTIONAL MATCH (a)-[*1..3]->(b:Robot {active: false}) \
             RETURN a, b";
    assert_eq!(rows(&fx, q), vec![vec![Some(s("s")), None]]);
    // A named path inside OPTIONAL MATCH.
    let q = "MATCH (a {id: 's'}) OPTIONAL MATCH p = (a)-[*1..3]->(b:Robot) \
             RETURN b, p.length AS n";
    assert_eq!(rows(&fx, q), vec![vec![Some(s("r1")), Some(Value::Int(1))]]);
}

#[test]
fn optional_match_with_a_variable_length_hop_that_finds_nothing_is_one_null_row() {
    // Released 0.9.x and QL-07 answered
    // `unsupported op inside OPTIONAL MATCH inner plan: VarExpand` here.
    let fx = fixture();
    // No path name, and nothing reachable at all: c1 has no out-edges.
    let q = "MATCH (a {id: 'c1'}) OPTIONAL MATCH (a)-[*1..3]->(b) RETURN a, b";
    assert_eq!(rows(&fx, q), vec![vec![Some(s("c1")), None]]);
    // No path name, a relationship-list variable, reachable nodes that all fail.
    let q = "MATCH (a {id: 'p2'}) OPTIONAL MATCH (a)-[r:KNOWS*]->(b:Robot) \
             RETURN a, b, r.length AS n";
    assert_eq!(rows(&fx, q), vec![vec![Some(s("p2")), None, None]]);
    // A named path, nothing reachable.
    let q = "MATCH (a {id: 'c1'}) OPTIONAL MATCH p = (a)-[*1..3]->(b) RETURN a, b, p";
    assert_eq!(rows(&fx, q), vec![vec![Some(s("c1")), None, None]]);
    // A named path, reachable nodes that all fail the destination filter.
    let q = "MATCH (a {id: 'p2'}) OPTIONAL MATCH p = (a)-[*1..2]->(b:Person {active: true}) \
             RETURN a, b, p";
    assert_eq!(rows(&fx, q), vec![vec![Some(s("p2")), None, None]]);
    // One input row matches, one does not: each keeps its own outcome.
    let q = "MATCH (a:City) WHERE a.id IN ['c1', 'c2'] \
             OPTIONAL MATCH p = (a)<-[:LIVES*1..2]-(b:Person {active: false}) \
             RETURN a, b, p.length AS n ORDER BY a";
    assert_eq!(
        rows(&fx, q),
        vec![
            vec![Some(s("c1")), None, None],
            vec![Some(s("c2")), Some(s("p2")), Some(Value::Int(1))]
        ]
    );
}

#[test]
fn a_named_path_and_a_relationship_list_variable_see_only_filtered_ends() {
    let fx = fixture();
    let q = "MATCH p = (a {id: 's'})-[*1..3]->(b:Person {active: true}) \
             RETURN p.nodes AS n ORDER BY b";
    let got: Vec<Option<Value>> = rows(&fx, q).into_iter().map(|r| r[0].clone()).collect();
    let list = |xs: &[&str]| Some(Value::List(xs.iter().map(|x| s(x)).collect()));
    assert_eq!(
        got,
        vec![
            list(&["s", "p1"]),
            list(&["s", "r1", "p3"]),
            list(&["s", "r1", "p3", "p5"])
        ]
    );
    let q = "MATCH (a {id: 's'})-[r*1..3]->(b:Person {active: true}) \
             RETURN b, r.length AS n ORDER BY b";
    assert_eq!(
        rows(&fx, q),
        vec![
            vec![Some(s("p1")), Some(Value::Int(1))],
            vec![Some(s("p3")), Some(Value::Int(2))],
            vec![Some(s("p5")), Some(Value::Int(3))]
        ]
    );
}

#[test]
fn parameters_in_the_destination_map_work() {
    let fx = fixture();
    let params = BTreeMap::from([("id".to_owned(), s("p5")), ("on".to_owned(), b(false))]);
    let q = "MATCH (a {id: 's'})-[*1..3]->(b {id: $id}) RETURN b";
    assert_eq!(rows_with(&fx, q, &params), vec![vec![Some(s("p5"))]]);
    let q = "MATCH (a {id: 's'})-[*1..3]->(b:Person {active: $on}) RETURN b";
    assert_eq!(rows_with(&fx, q, &params), vec![vec![Some(s("p2"))]]);
    // A missing parameter is the usual named error, not an empty result.
    let err = run(
        &fx.view(),
        "MATCH (a {id: 's'})-[*1..3]->(b {id: $nope}) RETURN b",
        &BTreeMap::new(),
    )
    .expect_err("missing parameter");
    assert!(err.contains("nope"), "{err}");
}

#[test]
fn aggregates_over_a_filtered_hop_count_only_matching_ends() {
    let fx = fixture();
    let q = "MATCH (a {id: 's'})-[*1..3]->(b:Person {active: true}) RETURN count(*) AS n";
    assert_eq!(rows(&fx, q), vec![vec![Some(Value::Int(3))]]);
    // Reachable Persons: p1, p3, p5 active; p2 inactive; p4 without the
    // property. One row per group, with that group's count.
    let q = "MATCH (a {id: 's'})-[*1..3]->(b:Person) RETURN b.active AS act, count(*) AS n";
    let mut got = rows(&fx, q);
    got.sort_by_key(|r| format!("{r:?}"));
    let mut want = vec![
        vec![Some(b(true)), Some(Value::Int(3))],
        vec![Some(b(false)), Some(Value::Int(1))],
        vec![None, Some(Value::Int(1))],
    ];
    want.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(got, want, "true / false / null groups and their counts");
    let q = "MATCH (a {id: 's'})-[*1..3]->(b:Person) \
             WITH b WHERE b.active = true RETURN count(b) AS n";
    assert_eq!(rows(&fx, q), vec![vec![Some(Value::Int(3))]]);
}

#[test]
fn a_bound_destination_is_rechecked_against_the_pattern() {
    let fx = fixture();
    // p3 is reachable, but the pattern says `:Robot`.
    let q = "MATCH (b {id: 'p3'}) MATCH (a {id: 's'})-[*1..3]->(b:Robot) RETURN b";
    assert!(rows(&fx, q).is_empty());
    let q = "MATCH (b {id: 'p3'}) MATCH (a {id: 's'})-[*1..3]->(b:Person {active: true}) RETURN b";
    assert_eq!(keys(&fx, q), some(&["p3"]));
}

#[test]
fn the_start_node_keeps_its_filter_when_the_end_is_bound_first() {
    let fx = fixture();
    // The end is bound; the planner keeps the walk left to right, so the
    // start's label and properties are applied by the start scan.
    let q = "MATCH (t {id: 'p5'}) MATCH (c:Robot)-[*1..3]->(t) RETURN c";
    assert_eq!(keys(&fx, q), some(&["r1"]));
    let q = "MATCH (t {id: 'p5'}) MATCH (c:Person {active: true})-[*1..4]->(t) RETURN c ORDER BY c";
    assert_eq!(keys(&fx, q), some(&["p3", "s", "x"]));
}

#[test]
fn shortest_path_rechecks_its_destination_filter() {
    let fx = fixture();
    let q = "MATCH (a {id: 's'}) MATCH (d {id: 'p5'}) \
             MATCH p = shortestPath((a)-[*..5]->(d:Robot)) RETURN p";
    assert!(rows(&fx, q).is_empty());
    let q = "MATCH (a {id: 's'}) MATCH (d {id: 'p5'}) \
             MATCH p = shortestPath((a)-[*..5]->(d:Person {active: true})) RETURN p.length AS n";
    assert_eq!(rows(&fx, q), vec![vec![Some(Value::Int(3))]]);
}

#[test]
fn an_index_on_the_destination_gives_the_same_rows_as_the_scan() {
    let fx = fixture();
    let mut pi = PropertyIndex::new();
    pi.enable("Person", "active");
    for key in ["s", "p1", "p3", "p5", "p6", "x"] {
        pi.set("Person", "active", fx.ids.get(key).unwrap(), &b(true));
    }
    pi.set("Person", "active", fx.ids.get("p2").unwrap(), &b(false));
    for q in [
        "MATCH (a {id: 's'})-[*1..3]->(b:Person {active: true}) RETURN a, b ORDER BY b",
        "MATCH (a {id: 's'})-[*1..3]->(b:Person {active: false}) RETURN a, b ORDER BY b",
        "MATCH (a:Person {active: true})-[*1..2]->(b:Person {active: true}) RETURN a, b ORDER BY a, b",
    ] {
        let scan = run(&fx.view(), q, &BTreeMap::new()).unwrap();
        let indexed = run(&fx.view_indexed(&pi), q, &BTreeMap::new()).unwrap();
        let all = |rs: &crate::result::ResultSet| -> Vec<Vec<Option<Value>>> {
            (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
        };
        assert_eq!(all(&indexed), all(&scan), "{q}");
        assert!(!scan.is_empty(), "{q}");
    }
}

// ---------------------------------------------------------------------------
// Randomized differential against a brute-force walk.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone)]
struct RefNode {
    key: String,
    label: &'static str,
    active: Option<bool>,
}

struct RefGraph {
    nodes: Vec<RefNode>,
    /// `(etype, src index, dst index)`, no duplicates, no self-loops.
    edges: Vec<(&'static str, usize, usize)>,
}

fn random_graph(rng: &mut Rng) -> RefGraph {
    let n = 3 + rng.below(6) as usize;
    let nodes: Vec<RefNode> = (0..n)
        .map(|i| RefNode {
            key: format!("n{i}"),
            label: if rng.below(2) == 0 { "A" } else { "B" },
            active: match rng.below(3) {
                0 => Some(true),
                1 => Some(false),
                _ => None,
            },
        })
        .collect();
    let mut seen = BTreeSet::new();
    let mut edges = Vec::new();
    for _ in 0..rng.below((n * 2) as u64 + 1) {
        let src = rng.below(n as u64) as usize;
        let dst = rng.below(n as u64) as usize;
        let et = if rng.below(2) == 0 { "X" } else { "Y" };
        if src != dst && seen.insert((et, src, dst)) {
            edges.push((et, src, dst));
        }
    }
    RefGraph { nodes, edges }
}

fn to_fx(g: &RefGraph) -> Fx {
    let mut fx = Fx::new();
    let ids: Vec<u32> = g
        .nodes
        .iter()
        .map(|n| {
            let props = n.active.map(|a| vec![("active", b(a))]).unwrap_or_default();
            fx.add(n.label, &n.key, props)
        })
        .collect();
    for &(et, src, dst) in &g.edges {
        fx.edge(et, ids[src], ids[dst], vec![]);
    }
    fx
}

#[derive(Clone, Copy)]
enum Dir {
    Right,
    Left,
    Both,
}

struct RefQuery {
    etype: Option<&'static str>,
    dir: Dir,
    min: usize,
    max: usize,
    label: Option<&'static str>,
    active: Option<bool>,
}

/// Every `(start, end)` pair, one per edge-unique walk, sorted.
fn brute_force(g: &RefGraph, q: &RefQuery) -> Vec<(String, String)> {
    fn walk(
        g: &RefGraph,
        q: &RefQuery,
        start: usize,
        at: usize,
        used: &mut Vec<usize>,
        out: &mut Vec<(String, String)>,
    ) {
        let depth = used.len();
        if depth >= q.min && depth >= 1 {
            let end = &g.nodes[at];
            let label_ok = q.label.is_none_or(|l| end.label == l);
            let props_ok = q.active.is_none_or(|a| end.active == Some(a));
            if label_ok && props_ok {
                out.push((g.nodes[start].key.clone(), end.key.clone()));
            }
        }
        if depth == q.max {
            return;
        }
        for (i, &(et, src, dst)) in g.edges.iter().enumerate() {
            if used.contains(&i) || q.etype.is_some_and(|t| t != et) {
                continue;
            }
            let next = match q.dir {
                Dir::Right if src == at => dst,
                Dir::Left if dst == at => src,
                Dir::Both if src == at => dst,
                Dir::Both if dst == at => src,
                _ => continue,
            };
            used.push(i);
            walk(g, q, start, next, used, out);
            used.pop();
        }
    }
    let mut out = Vec::new();
    for start in 0..g.nodes.len() {
        walk(g, q, start, start, &mut Vec::new(), &mut out);
    }
    out.sort();
    out
}

fn cypher_for(q: &RefQuery) -> String {
    let etype = q.etype.map(|t| format!(":{t}")).unwrap_or_default();
    let range = format!("*{}..{}", q.min, q.max);
    let (left, right) = match q.dir {
        Dir::Right => ("-", "->"),
        Dir::Left => ("<-", "-"),
        Dir::Both => ("-", "-"),
    };
    let label = q.label.map(|l| format!(":{l}")).unwrap_or_default();
    let props = q
        .active
        .map(|a| format!(" {{active: {a}}}"))
        .unwrap_or_default();
    format!("MATCH (a){left}[{etype}{range}]{right}(b{label}{props}) RETURN a, b")
}

fn engine(fx: &Fx, cypher: &str) -> Vec<(String, String)> {
    let mut got: Vec<(String, String)> = rows(fx, cypher)
        .into_iter()
        .map(|row| match (&row[0], &row[1]) {
            (Some(Value::Str(a)), Some(Value::Str(b))) => (a.clone(), b.clone()),
            other => panic!("`{cypher}`: unexpected row {other:?}"),
        })
        .collect();
    got.sort();
    got
}

#[test]
fn randomized_graphs_match_a_brute_force_walk() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut filtered_cases = 0usize;
    for _ in 0..150 {
        let g = random_graph(&mut rng);
        let fx = to_fx(&g);
        for _ in 0..8 {
            let min = 1 + rng.below(3) as usize;
            let q = RefQuery {
                etype: [None, Some("X"), Some("Y")][rng.below(3) as usize],
                dir: [Dir::Right, Dir::Left, Dir::Both][rng.below(3) as usize],
                min,
                max: min + rng.below(3) as usize,
                label: [None, Some("A"), Some("B")][rng.below(3) as usize],
                active: [None, Some(true), Some(false)][rng.below(3) as usize],
            };
            let cypher = cypher_for(&q);
            let want = brute_force(&g, &q);
            let got = engine(&fx, &cypher);
            assert_eq!(got, want, "`{cypher}` on edges {:?}", g.edges);
            if q.label.is_some() || q.active.is_some() {
                filtered_cases += 1;
            }
        }
    }
    assert!(filtered_cases > 500, "{filtered_cases}");
}
