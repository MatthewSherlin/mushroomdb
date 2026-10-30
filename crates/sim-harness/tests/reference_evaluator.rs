//! H-3 (0.10.x hardening): the engine answers a core of Cypher as an
//! independent reference evaluator does.
//!
//! H-1, H-2, H-4 and H-5 check the engine against itself: one physical state
//! against another, a query against its rewrite, a past commit against the
//! live store. A construct that is wrong in every state and every rewrite at
//! once passes all of them. This test checks the engine against an answer it
//! had no part in.
//!
//! A query is generated as a small syntax tree (the enums below), rendered to
//! Cypher text for the engine, and evaluated directly over a plain in-memory
//! graph by the evaluator in this file. The evaluator is written from the
//! openCypher semantics and shares nothing with `core-query`: no parser,
//! planner or evaluator. The generated subset:
//!
//! - `MATCH` of a node, a one-hop or a two-hop directed path, each node and
//!   relationship with an optional label or type, and an optional second,
//!   comma-separated pattern: a fresh node, or a hop from a node already
//!   bound. No relationship is bound twice in one `MATCH`.
//! - `WHERE` in three-valued logic: `=`, `<>`, `<`, `<=`, `>`, `>=` between a
//!   property, key or variable and a literal or another such term, over
//!   values of mixed type and missing properties; `AND`, `OR`, `XOR`, `NOT`,
//!   `IS NULL`, `IS NOT NULL`, `IN` a literal list.
//! - `UNWIND` of a literal list, and one `WITH` stage, plain or aggregating,
//!   with or without `DISTINCT` and its own `WHERE`.
//! - `RETURN` of keys, properties and variables, `DISTINCT`, and `count(*)`,
//!   `count`, `sum`, `avg`, `min`, `max` and `collect` (each optionally
//!   `DISTINCT`) with implicit grouping keys.
//! - `ORDER BY` every orderable projected column, so the order is total on
//!   the rows; `SKIP`; `LIMIT`.
//!
//! The graph (up to 30 nodes, two labels, two edge types, properties of mixed
//! type and missing ones) is written through the public `GraphDb` API with a
//! snapshot at a random point and a few property changes after it, so reads
//! cross a snapshot base and an overlay.
//!
//! Answers are compared as multisets of rows, or as sequences under the total
//! `ORDER BY`. Without `ORDER BY`, `SKIP` and `LIMIT` may pick any rows, so the
//! row count and membership are checked. A float from `sum` or `avg` is
//! compared to nine significant digits; everything else is exact, so an
//! integer is never equal to a float.
//!
//! Where the engine deliberately differs from openCypher and documents it,
//! the documented behaviour is one named function that points at its source:
//! `sum_of_nothing` (ledger row 166). Every other difference is an engine bug.
//! Three found when this test was written are ledger rows 168–170, pinned,
//! shrunk, as the `finding_*` tests at the end. Rows 168 and 169 are fixed;
//! row 170's test is ignored until 0.10.1 fixes it, and the property skips
//! its shape in `hits_row_170`.
//! Shapes the engine refuses outright (`WITH DISTINCT`;
//! `RETURN DISTINCT` with an aggregate) are not generated.
//! Untyped hops (`-->`, row 171 fixed) alternate between the bare form and
//! the bracketed `-[]->` form, so both spellings are exercised.
//!
//! Any divergence is an engine bug. Do not weaken these assertions.
//!
//! Cases: 48 per run by default; set `H3_CASES` to raise it (the nightly run).

use mushroomdb::{GraphDb, ResultSet, Value};
use proptest::prelude::*;
use sim_harness::SimFs;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

const LABELS: [&str; 2] = ["A", "B"];
const ETYPES: [&str; 2] = ["R", "S"];
/// `i` integer, `f` float, `x` integer or float, `m` any type, `s` string,
/// `b` boolean. `z` is never written, so it is always missing.
const PROPS: [&str; 7] = ["i", "f", "x", "m", "s", "b", "z"];
const WRITTEN_PROPS: usize = 6;
const STRS: [&str; 3] = ["a", "b", "c"];

// ---------------------------------------------------------------------------
// Values and the reference graph
// ---------------------------------------------------------------------------

/// A value in the reference graph or in a reference row.
#[derive(Debug, Clone, PartialEq)]
enum Val {
    Null,
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    List(Vec<Val>),
    Node(usize),
}

/// Every float the data holds ends in `.5`, so no stored float equals an
/// integer: grouping, `DISTINCT` and sort ties never have to choose between
/// `1` and `1.0`.
fn prop_value(prop: usize, seed: u8) -> Val {
    let half = |v: u8| Val::Float(f64::from(v % 6) - 2.0 + 0.5);
    match prop {
        0 => Val::Int(i64::from(seed % 7) - 2),
        1 => half(seed),
        2 => {
            if seed.is_multiple_of(2) {
                Val::Int(i64::from(seed / 2 % 6) - 1)
            } else {
                half(seed / 2)
            }
        }
        3 => match seed % 4 {
            0 => Val::Int(i64::from(seed / 4 % 5) - 1),
            1 => half(seed / 4),
            2 => Val::Str(STRS[usize::from(seed / 4 % 3)].into()),
            _ => Val::Bool(seed / 4 % 2 == 1),
        },
        4 => Val::Str(STRS[usize::from(seed % 3)].into()),
        _ => Val::Bool(seed % 2 == 1),
    }
}

struct GNode {
    label: usize,
    key: String,
    props: BTreeMap<&'static str, Val>,
}

/// The logical graph: what a correct engine holds after the writes.
struct Graph {
    nodes: Vec<GNode>,
    /// `(type, src, dst)`, each at most once.
    edges: Vec<(usize, usize, usize)>,
}

#[derive(Debug, Clone)]
struct NodeSpec {
    label: u8,
    props: Vec<Option<u8>>,
}

/// A property change after the inserts: `Some(seed)` sets, `None` removes.
#[derive(Debug, Clone)]
struct Update {
    node: u8,
    prop: u8,
    value: Option<u8>,
}

fn node_strategy() -> impl Strategy<Value = NodeSpec> {
    (
        any::<u8>(),
        proptest::collection::vec(proptest::option::weighted(0.75, any::<u8>()), WRITTEN_PROPS),
    )
        .prop_map(|(label, props)| NodeSpec { label, props })
}

fn update_strategy() -> impl Strategy<Value = Update> {
    (
        any::<u8>(),
        any::<u8>(),
        proptest::option::weighted(0.7, any::<u8>()),
    )
        .prop_map(|(node, prop, value)| Update { node, prop, value })
}

fn key_of(i: usize) -> String {
    format!("n{i:02}")
}

fn to_value(v: &Val) -> Value {
    match v {
        Val::Int(i) => Value::Int(*i),
        Val::Float(f) => Value::Float(*f),
        Val::Str(s) => Value::Str(s.clone()),
        Val::Bool(b) => Value::Bool(*b),
        other => unreachable!("not a stored value: {other:?}"),
    }
}

fn from_value(v: Option<&Value>) -> Val {
    match v {
        None | Some(Value::Null) => Val::Null,
        Some(Value::Int(i)) => Val::Int(*i),
        Some(Value::Float(f)) => Val::Float(*f),
        Some(Value::Str(s)) => Val::Str(s.clone()),
        Some(Value::Bool(b)) => Val::Bool(*b),
        Some(Value::List(xs)) => Val::List(xs.iter().map(|x| from_value(Some(x))).collect()),
        Some(other) => Val::Str(format!("<unexpected {other:?}>")),
    }
}

/// Write the graph to a fresh engine store and to the reference graph. The
/// writes are the node inserts, then the edge inserts, then the property
/// changes; the store snapshots before write `snapshot_slot` (mod the count
/// plus one, so "never" and "after the last write" both occur).
fn build(
    nodes: &[NodeSpec],
    edges: &[(u8, u8, u8)],
    updates: &[Update],
    snapshot_slot: usize,
) -> (Graph, GraphDb<SimFs>) {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();
    let mut g = Graph {
        nodes: Vec::new(),
        edges: Vec::new(),
    };
    let total = nodes.len() + edges.len() + updates.len();
    let snap_at = snapshot_slot % (total + 1);
    let mut step = 0;
    let mut tick = |db: &mut GraphDb<SimFs>| {
        if step == snap_at {
            db.snapshot().expect("snapshot");
        }
        step += 1;
    };
    for (idx, n) in nodes.iter().enumerate() {
        tick(&mut db);
        let props: BTreeMap<&'static str, Val> = n
            .props
            .iter()
            .enumerate()
            .filter_map(|(p, s)| s.map(|s| (PROPS[p], prop_value(p, s))))
            .collect();
        let label = usize::from(n.label % 2);
        db.insert_node(
            LABELS[label],
            &key_of(idx),
            props
                .iter()
                .map(|(k, v)| (k.to_string(), to_value(v)))
                .collect(),
        )
        .expect("insert_node");
        g.nodes.push(GNode {
            label,
            key: key_of(idx),
            props,
        });
    }
    let count = nodes.len();
    for &(t, s, d) in edges {
        tick(&mut db);
        let e = (
            usize::from(t % 2),
            usize::from(s) % count,
            usize::from(d) % count,
        );
        db.insert_edge(ETYPES[e.0], &key_of(e.1), &key_of(e.2))
            .expect("insert_edge");
        if !g.edges.contains(&e) {
            g.edges.push(e);
        }
    }
    for u in updates {
        tick(&mut db);
        let node = usize::from(u.node) % count;
        let prop = usize::from(u.prop) % WRITTEN_PROPS;
        match u.value {
            Some(seed) => {
                let v = prop_value(prop, seed);
                db.set_prop(&key_of(node), PROPS[prop], to_value(&v))
                    .expect("set_prop");
                g.nodes[node].props.insert(PROPS[prop], v);
            }
            None => {
                db.remove_prop(&key_of(node), PROPS[prop])
                    .expect("remove_prop");
                g.nodes[node].props.remove(PROPS[prop]);
            }
        }
    }
    tick(&mut db);
    (g, db)
}

fn dump(g: &Graph) -> String {
    let mut out = Vec::new();
    for n in &g.nodes {
        let props: Vec<String> = n
            .props
            .iter()
            .map(|(k, v)| format!("{k}: {}", render_val(v)))
            .collect();
        out.push(format!(
            "({}:{} {{{}}})",
            n.key,
            LABELS[n.label],
            props.join(", ")
        ));
    }
    for &(t, s, d) in &g.edges {
        out.push(format!(
            "({})-[:{}]->({})",
            g.nodes[s].key, ETYPES[t], g.nodes[d].key
        ));
    }
    out.join("\n  ")
}

// ---------------------------------------------------------------------------
// The query syntax tree and its rendering
// ---------------------------------------------------------------------------

/// What a name in scope holds, as far as the generator knows. `MixedSum` is a
/// `sum` over integers and floats together: it can be an integer or a float
/// with an integral value, which group and sort as one value but print as
/// two, so it is never a grouping key, a `DISTINCT` column or a sort key.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Int,
    Float,
    Num,
    Str,
    Bool,
    Any,
    Null,
    Node,
    MixedSum,
    List,
}

fn prop_kind(p: &str) -> Kind {
    match p {
        "i" => Kind::Int,
        "f" => Kind::Float,
        "x" => Kind::Num,
        "m" => Kind::Any,
        "s" => Kind::Str,
        "b" => Kind::Bool,
        _ => Kind::Null,
    }
}

fn is_numeric(k: Kind) -> bool {
    matches!(
        k,
        Kind::Int | Kind::Float | Kind::Num | Kind::Null | Kind::MixedSum
    )
}

#[derive(Debug, Clone)]
enum Term {
    Prop(String, &'static str),
    Key(String),
    Var(String),
}

#[derive(Debug, Clone)]
enum Operand {
    Lit(Val),
    Term(Term),
}

#[derive(Debug, Clone)]
enum Pred {
    Cmp(Term, &'static str, Operand),
    /// `IS NULL`, or `IS NOT NULL` when the flag is set.
    IsNull(Term, bool),
    In(Term, Vec<Val>),
    Not(Box<Pred>),
    And(Box<Pred>, Box<Pred>),
    Or(Box<Pred>, Box<Pred>),
    Xor(Box<Pred>, Box<Pred>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AggFn {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
}

#[derive(Debug, Clone)]
enum Item {
    /// A node variable carried through `WITH` under its own name.
    Node(String),
    Term(Term),
    CountStar,
    Agg(AggFn, bool, Term),
}

impl Item {
    fn is_agg(&self) -> bool {
        matches!(self, Item::CountStar | Item::Agg(..))
    }
}

#[derive(Debug, Clone)]
struct Projection {
    distinct: bool,
    items: Vec<(Item, String, Kind)>,
}

/// A path pattern. A node whose variable is already bound has no label.
#[derive(Debug, Clone)]
struct Path {
    nodes: Vec<(String, Option<usize>)>,
    rels: Vec<Option<usize>>,
    /// For each untyped (`None`) rel, whether to render the bare form (`-->`
    /// / `<--` / `--`) instead of the bracketed form (`-[]->`). Typed rels
    /// always render with brackets. If shorter than `rels`, remaining entries
    /// default to `false`.
    bare_rels: Vec<bool>,
}

#[derive(Debug, Clone)]
struct Query {
    paths: Vec<Path>,
    filter: Option<Pred>,
    unwind: Option<Vec<Val>>,
    with: Option<(Projection, Option<Pred>)>,
    ret: Projection,
    order: Vec<(usize, bool)>,
    skip: Option<usize>,
    limit: Option<usize>,
}

fn render_val(v: &Val) -> String {
    match v {
        Val::Null => "null".into(),
        Val::Int(i) => i.to_string(),
        Val::Float(f) => format!("{f:?}"),
        Val::Str(s) => format!("'{s}'"),
        Val::Bool(b) => b.to_string(),
        Val::List(xs) => format!(
            "[{}]",
            xs.iter().map(render_val).collect::<Vec<_>>().join(", ")
        ),
        Val::Node(i) => format!("<node {i}>"),
    }
}

fn render_term(t: &Term) -> String {
    match t {
        Term::Prop(v, p) => format!("{v}.{p}"),
        Term::Key(v) => format!("key({v})"),
        Term::Var(v) => v.clone(),
    }
}

fn render_pred(p: &Pred) -> String {
    match p {
        Pred::Cmp(t, op, rhs) => {
            let rhs = match rhs {
                Operand::Lit(v) => render_val(v),
                Operand::Term(t) => render_term(t),
            };
            format!("{} {op} {rhs}", render_term(t))
        }
        Pred::IsNull(t, false) => format!("{} IS NULL", render_term(t)),
        Pred::IsNull(t, true) => format!("{} IS NOT NULL", render_term(t)),
        Pred::In(t, xs) => format!(
            "{} IN {}",
            render_term(t),
            render_val(&Val::List(xs.clone()))
        ),
        Pred::Not(a) => format!("NOT ({})", render_pred(a)),
        Pred::And(a, b) => format!("({}) AND ({})", render_pred(a), render_pred(b)),
        Pred::Or(a, b) => format!("({}) OR ({})", render_pred(a), render_pred(b)),
        Pred::Xor(a, b) => format!("({}) XOR ({})", render_pred(a), render_pred(b)),
    }
}

fn render_path(p: &Path) -> String {
    let node = |(v, l): &(String, Option<usize>)| match l {
        Some(l) => format!("({v}:{})", LABELS[*l]),
        None => format!("({v})"),
    };
    let mut out = node(&p.nodes[0]);
    for (i, r) in p.rels.iter().enumerate() {
        match r {
            Some(t) => out.push_str(&format!("-[:{}]->", ETYPES[*t])),
            None => {
                // Bare `-->` (row 171 fix): alternate between bare and bracketed
                // so H-3 covers both forms.
                if p.bare_rels.get(i).copied().unwrap_or(false) {
                    out.push_str("-->");
                } else {
                    out.push_str("-[]->");
                }
            }
        }
        out.push_str(&node(&p.nodes[i + 1]));
    }
    out
}

fn render_projection(p: &Projection) -> String {
    let items: Vec<String> = p
        .items
        .iter()
        .map(|(item, alias, _)| match item {
            Item::Node(v) => v.clone(),
            Item::Term(t) => format!("{} AS {alias}", render_term(t)),
            Item::CountStar => format!("count(*) AS {alias}"),
            Item::Agg(f, distinct, arg) => {
                let name = match f {
                    AggFn::Count => "count",
                    AggFn::Sum => "sum",
                    AggFn::Avg => "avg",
                    AggFn::Min => "min",
                    AggFn::Max => "max",
                    AggFn::Collect => "collect",
                };
                let d = if *distinct { "DISTINCT " } else { "" };
                format!("{name}({d}{}) AS {alias}", render_term(arg))
            }
        })
        .collect();
    let d = if p.distinct { "DISTINCT " } else { "" };
    format!("{d}{}", items.join(", "))
}

fn render_query(q: &Query) -> String {
    let paths: Vec<String> = q.paths.iter().map(render_path).collect();
    let mut out = format!("MATCH {}", paths.join(", "));
    if let Some(p) = &q.filter {
        out.push_str(&format!(" WHERE {}", render_pred(p)));
    }
    if let Some(xs) = &q.unwind {
        out.push_str(&format!(
            " UNWIND {} AS u",
            render_val(&Val::List(xs.clone()))
        ));
    }
    if let Some((proj, filter)) = &q.with {
        out.push_str(&format!(" WITH {}", render_projection(proj)));
        if let Some(p) = filter {
            out.push_str(&format!(" WHERE {}", render_pred(p)));
        }
    }
    out.push_str(&format!(" RETURN {}", render_projection(&q.ret)));
    if !q.order.is_empty() {
        let keys: Vec<String> = q
            .order
            .iter()
            .map(|(col, desc)| {
                let alias = &q.ret.items[*col].1;
                if *desc {
                    format!("{alias} DESC")
                } else {
                    alias.clone()
                }
            })
            .collect();
        out.push_str(&format!(" ORDER BY {}", keys.join(", ")));
    }
    if let Some(s) = q.skip {
        out.push_str(&format!(" SKIP {s}"));
    }
    if let Some(l) = q.limit {
        out.push_str(&format!(" LIMIT {l}"));
    }
    out
}

// ---------------------------------------------------------------------------
// The generator
// ---------------------------------------------------------------------------

/// Draws choices from a byte string. Past its end every choice is 0, and
/// choice 0 is always the simplest option, so proptest shrinking the bytes
/// towards zero and towards shorter shrinks the query.
struct Gen<'a> {
    choices: &'a [u8],
    pos: usize,
}

impl Gen<'_> {
    fn pick(&mut self, n: usize) -> usize {
        let c = self.choices.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        usize::from(c) % n
    }

    fn flag(&mut self) -> bool {
        self.pick(2) == 1
    }

    fn label(&mut self) -> Option<usize> {
        self.pick(3).checked_sub(1)
    }
}

type Scope = Vec<(String, Kind)>;

fn literal(i: usize) -> Val {
    match i % 15 {
        0 => Val::Int(1),
        1 => Val::Int(0),
        2 => Val::Int(2),
        3 => Val::Int(-1),
        4 => Val::Int(4),
        5 => Val::Float(1.5),
        6 => Val::Float(2.0),
        7 => Val::Float(-0.5),
        8 => Val::Str("a".into()),
        9 => Val::Str("b".into()),
        10 => Val::Str("z".into()),
        11 => Val::Str("n10".into()),
        12 => Val::Bool(true),
        13 => Val::Bool(false),
        _ => Val::Null,
    }
}

/// A scalar term over the scope. `projectable` leaves out `MixedSum` names.
fn gen_term(g: &mut Gen, scope: &Scope, projectable: bool) -> Option<(Term, Kind)> {
    let nodes: Vec<&String> = scope
        .iter()
        .filter(|(_, k)| *k == Kind::Node)
        .map(|(n, _)| n)
        .collect();
    let vars: Vec<&(String, Kind)> = scope
        .iter()
        .filter(|(_, k)| {
            *k != Kind::Node && *k != Kind::List && !(projectable && *k == Kind::MixedSum)
        })
        .collect();
    let use_var = !vars.is_empty() && (nodes.is_empty() || g.pick(3) == 2);
    if use_var {
        let (name, kind) = vars[g.pick(vars.len())];
        return Some((Term::Var(name.clone()), *kind));
    }
    if nodes.is_empty() {
        return None;
    }
    let node = nodes[g.pick(nodes.len())].clone();
    let p = g.pick(PROPS.len() + 1);
    Some(if p == PROPS.len() {
        (Term::Key(node), Kind::Str)
    } else {
        (Term::Prop(node, PROPS[p]), prop_kind(PROPS[p]))
    })
}

fn gen_pred(g: &mut Gen, scope: &Scope, depth: usize) -> Pred {
    let shape = if depth == 0 { 0 } else { g.pick(9) };
    let sub = |g: &mut Gen| Box::new(gen_pred(g, scope, depth - 1));
    match shape {
        5 => Pred::Not(sub(g)),
        6 => Pred::And(sub(g), sub(g)),
        7 => Pred::Or(sub(g), sub(g)),
        8 => Pred::Xor(sub(g), sub(g)),
        _ => {
            let (t, _) = gen_term(g, scope, false).expect("a scope always has a term");
            match g.pick(6) {
                3 => Pred::IsNull(t, false),
                4 => Pred::IsNull(t, true),
                5 => {
                    let n = g.pick(4);
                    Pred::In(t, (0..n).map(|_| literal(g.pick(15))).collect())
                }
                k => {
                    let op = ["=", "<>", "<", "<=", ">", ">="][g.pick(6)];
                    let rhs = if k == 2 {
                        Operand::Term(gen_term(g, scope, false).unwrap().0)
                    } else {
                        Operand::Lit(literal(g.pick(15)))
                    };
                    Pred::Cmp(t, op, rhs)
                }
            }
        }
    }
}

fn kind_of_values(xs: &[Val]) -> Kind {
    let kinds: BTreeSet<&str> = xs
        .iter()
        .filter_map(|v| match v {
            Val::Int(_) => Some("int"),
            Val::Float(_) => Some("float"),
            Val::Str(_) => Some("str"),
            Val::Bool(_) => Some("bool"),
            _ => None,
        })
        .collect();
    match kinds.iter().copied().collect::<Vec<_>>().as_slice() {
        [] => Kind::Null,
        ["int"] => Kind::Int,
        ["float"] => Kind::Float,
        ["float", "int"] => Kind::Num,
        ["str"] => Kind::Str,
        ["bool"] => Kind::Bool,
        _ => Kind::Any,
    }
}

fn gen_unwind(g: &mut Gen) -> Vec<Val> {
    let numeric = !g.flag();
    let n = 1 + g.pick(3);
    (0..n)
        .map(|_| {
            if numeric {
                [
                    Val::Int(1),
                    Val::Int(2),
                    Val::Float(0.5),
                    Val::Null,
                    Val::Int(-1),
                ][g.pick(5)]
                .clone()
            } else {
                [
                    Val::Int(1),
                    Val::Str("a".into()),
                    Val::Bool(true),
                    Val::Float(2.5),
                    Val::Null,
                    Val::Str("b".into()),
                ][g.pick(6)]
                .clone()
            }
        })
        .collect()
}

/// One aggregate over the scope, or `count(*)` when no argument fits.
fn gen_agg(g: &mut Gen, scope: &Scope, allow_collect: bool) -> (Item, Kind) {
    let f = match g.pick(7) {
        1 => AggFn::Count,
        2 => AggFn::Sum,
        3 => AggFn::Avg,
        4 => AggFn::Min,
        5 => AggFn::Max,
        6 if allow_collect => AggFn::Collect,
        6 => AggFn::Count,
        _ => return (Item::CountStar, Kind::Int),
    };
    let distinct = g.pick(4) == 3;
    let fits = |k: Kind| match f {
        AggFn::Count => k != Kind::List,
        AggFn::Sum | AggFn::Avg => is_numeric(k),
        _ => k != Kind::Node && k != Kind::List,
    };
    let mut args: Vec<(Term, Kind)> = Vec::new();
    for (name, kind) in scope {
        if *kind == Kind::Node {
            for p in PROPS {
                if fits(prop_kind(p)) {
                    args.push((Term::Prop(name.clone(), p), prop_kind(p)));
                }
            }
        }
        if fits(*kind) {
            args.push((Term::Var(name.clone()), *kind));
        }
    }
    if args.is_empty() {
        return (Item::CountStar, Kind::Int);
    }
    let (arg, k) = args.swap_remove(g.pick(args.len()));
    let out = match f {
        AggFn::Count => Kind::Int,
        AggFn::Sum => match k {
            Kind::Int => Kind::Int,
            Kind::Float => Kind::Float,
            Kind::Null => Kind::Null,
            _ => Kind::MixedSum,
        },
        AggFn::Avg => Kind::Float,
        AggFn::Collect => Kind::List,
        AggFn::Min | AggFn::Max => k,
    };
    (Item::Agg(f, distinct, arg), out)
}

/// A `WITH` (`is_with`) or `RETURN` projection, and the scope after it.
fn gen_projection(g: &mut Gen, scope: &Scope, is_with: bool) -> (Projection, Scope) {
    let prefix = if is_with { "w" } else { "c" };
    let aggregating = g.flag();
    let mut items: Vec<(Item, String, Kind)> = Vec::new();
    if is_with {
        for (name, kind) in scope {
            if *kind == Kind::Node && g.flag() {
                items.push((Item::Node(name.clone()), name.clone(), Kind::Node));
            }
        }
    }
    let terms = if aggregating {
        g.pick(3)
    } else {
        1 + g.pick(3)
    };
    for _ in 0..terms {
        if let Some((t, k)) = gen_term(g, scope, true) {
            let alias = format!("{prefix}{}", items.len());
            items.push((Item::Term(t), alias, k));
        }
    }
    if aggregating {
        for _ in 0..1 + g.pick(2) {
            let (item, k) = gen_agg(g, scope, !is_with);
            let alias = format!("{prefix}{}", items.len());
            items.push((item, alias, k));
        }
    }
    if items.is_empty() {
        items.push((Item::CountStar, format!("{prefix}0"), Kind::Int));
    }
    // The engine refuses `WITH DISTINCT` (a parse error) and `RETURN DISTINCT`
    // with an aggregate (a named plan error), so neither is generated.
    let distinct = !is_with && !items.iter().any(|i| i.0.is_agg()) && g.flag();
    let out = items.iter().map(|(_, a, k)| (a.clone(), *k)).collect();
    (Projection { distinct, items }, out)
}

fn gen_query(choices: &[u8]) -> Query {
    let mut g = Gen { choices, pos: 0 };
    let names = ["a", "b", "c"];
    let mut scope: Scope = Vec::new();
    let hops = g.pick(3);
    let mut first = Path {
        nodes: Vec::new(),
        rels: Vec::new(),
        bare_rels: Vec::new(),
    };
    for (h, name) in names.iter().enumerate().take(hops + 1) {
        first.nodes.push((name.to_string(), g.label()));
        scope.push((name.to_string(), Kind::Node));
        if h < hops {
            first.rels.push(g.label());
            first.bare_rels.push(g.flag());
        }
    }
    let mut paths = vec![first];
    match g.pick(4) {
        1 if hops <= 1 => {
            paths.push(Path {
                nodes: vec![("d".into(), g.label())],
                rels: Vec::new(),
                bare_rels: Vec::new(),
            });
            scope.push(("d".into(), Kind::Node));
        }
        2 => {
            let from = names[g.pick(hops + 1)].to_string();
            let rel = g.label();
            let bare = g.flag();
            paths.push(Path {
                nodes: vec![(from, None), ("e".into(), g.label())],
                rels: vec![rel],
                bare_rels: vec![bare],
            });
            scope.push(("e".into(), Kind::Node));
        }
        _ => {}
    }
    let filter = if g.flag() {
        Some(gen_pred(&mut g, &scope, 2))
    } else {
        None
    };
    let unwind = if g.pick(3) == 2 {
        let xs = gen_unwind(&mut g);
        scope.push(("u".into(), kind_of_values(&xs)));
        Some(xs)
    } else {
        None
    };
    let with = if g.pick(3) == 2 {
        let (proj, next) = gen_projection(&mut g, &scope, true);
        scope = next;
        let filter = if g.flag() {
            Some(gen_pred(&mut g, &scope, 1))
        } else {
            None
        };
        Some((proj, filter))
    } else {
        None
    };
    let (ret, _) = gen_projection(&mut g, &scope, false);
    let mut order = Vec::new();
    if g.flag() {
        let mut cols: Vec<usize> = (0..ret.items.len())
            .filter(|&i| ret.items[i].2 != Kind::List)
            .collect();
        while !cols.is_empty() {
            let col = cols.remove(g.pick(cols.len()));
            order.push((col, g.flag()));
        }
    }
    let skip = if g.pick(4) == 3 {
        Some(g.pick(5))
    } else {
        None
    };
    let limit = if g.pick(4) == 3 {
        Some(g.pick(8))
    } else {
        None
    };
    Query {
        paths,
        filter,
        unwind,
        with,
        ret,
        order,
        skip,
        limit,
    }
}

// ---------------------------------------------------------------------------
// The reference evaluator, from openCypher
// ---------------------------------------------------------------------------

type Row = BTreeMap<String, Val>;

fn match_paths(g: &Graph, paths: &[Path]) -> Vec<Row> {
    // Each row carries the relationships it bound: one MATCH never binds a
    // relationship twice, across all of its comma-separated parts.
    let mut rows: Vec<(Row, Vec<usize>)> = vec![(Row::new(), Vec::new())];
    for p in paths {
        let mut next = Vec::new();
        for (row, used) in rows {
            let (var, label) = &p.nodes[0];
            let starts: Vec<usize> = match row.get(var) {
                Some(Val::Node(i)) => vec![*i],
                _ => (0..g.nodes.len()).collect(),
            };
            for i in starts {
                if label.is_some_and(|l| g.nodes[i].label != l) {
                    continue;
                }
                let mut r = row.clone();
                r.insert(var.clone(), Val::Node(i));
                extend(g, p, 0, i, r, used.clone(), &mut next);
            }
        }
        rows = next;
    }
    rows.into_iter().map(|(r, _)| r).collect()
}

fn extend(
    g: &Graph,
    p: &Path,
    hop: usize,
    cur: usize,
    row: Row,
    used: Vec<usize>,
    out: &mut Vec<(Row, Vec<usize>)>,
) {
    if hop == p.rels.len() {
        out.push((row, used));
        return;
    }
    let (var, label) = &p.nodes[hop + 1];
    for (ei, &(t, s, d)) in g.edges.iter().enumerate() {
        if s != cur
            || used.contains(&ei)
            || p.rels[hop].is_some_and(|x| x != t)
            || label.is_some_and(|l| g.nodes[d].label != l)
        {
            continue;
        }
        if let Some(Val::Node(bound)) = row.get(var) {
            if *bound != d {
                continue;
            }
        }
        let mut r = row.clone();
        r.insert(var.clone(), Val::Node(d));
        let mut u = used.clone();
        u.push(ei);
        extend(g, p, hop + 1, d, r, u, out);
    }
}

fn eval_term(g: &Graph, row: &Row, t: &Term) -> Val {
    match t {
        Term::Prop(v, p) => match row.get(v) {
            Some(Val::Node(i)) => g.nodes[*i].props.get(p).cloned().unwrap_or(Val::Null),
            _ => Val::Null,
        },
        Term::Key(v) => match row.get(v) {
            Some(Val::Node(i)) => Val::Str(g.nodes[*i].key.clone()),
            _ => Val::Null,
        },
        Term::Var(v) => row.get(v).cloned().unwrap_or(Val::Null),
    }
}

fn as_f64(v: &Val) -> Option<f64> {
    match v {
        Val::Int(i) => Some(*i as f64),
        Val::Float(f) => Some(*f),
        _ => None,
    }
}

/// openCypher `=`: null if either side is null; numbers by value, so `1 = 1.0`;
/// values of different types are unequal.
fn cypher_eq(a: &Val, b: &Val) -> Option<bool> {
    match (a, b) {
        (Val::Null, _) | (_, Val::Null) => None,
        (Val::Int(x), Val::Int(y)) => Some(x == y),
        (Val::Int(_) | Val::Float(_), Val::Int(_) | Val::Float(_)) => Some(as_f64(a) == as_f64(b)),
        (Val::Str(x), Val::Str(y)) => Some(x == y),
        (Val::Bool(x), Val::Bool(y)) => Some(x == y),
        (Val::Node(x), Val::Node(y)) => Some(x == y),
        (Val::List(xs), Val::List(ys)) => {
            if xs.len() != ys.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (x, y) in xs.iter().zip(ys) {
                match cypher_eq(x, y) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown {
                None
            } else {
                Some(true)
            }
        }
        _ => Some(false),
    }
}

/// openCypher comparability for `<`, `<=`, `>`, `>=`: numbers, strings and
/// booleans (`false < true`) compare within their type; any other pair, and
/// any null, is null.
fn cypher_cmp(a: &Val, b: &Val) -> Option<Ordering> {
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => Some(x.cmp(y)),
        (Val::Int(_) | Val::Float(_), Val::Int(_) | Val::Float(_)) => {
            as_f64(a)?.partial_cmp(&as_f64(b)?)
        }
        (Val::Str(x), Val::Str(y)) => Some(x.cmp(y)),
        (Val::Bool(x), Val::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// openCypher orderability, used by `ORDER BY`, `min` and `max`: ascending,
/// lists, then strings, then booleans, then numbers, then null.
fn orderability(a: &Val, b: &Val) -> Ordering {
    fn rank(v: &Val) -> u8 {
        match v {
            Val::Node(_) => 0,
            Val::List(_) => 1,
            Val::Str(_) => 2,
            Val::Bool(_) => 3,
            Val::Int(_) | Val::Float(_) => 4,
            Val::Null => 5,
        }
    }
    match rank(a).cmp(&rank(b)) {
        Ordering::Equal => {}
        o => return o,
    }
    match (a, b) {
        (Val::List(xs), Val::List(ys)) => {
            for (x, y) in xs.iter().zip(ys) {
                match orderability(x, y) {
                    Ordering::Equal => {}
                    o => return o,
                }
            }
            xs.len().cmp(&ys.len())
        }
        (Val::Node(x), Val::Node(y)) => x.cmp(y),
        (Val::Null, Val::Null) => Ordering::Equal,
        _ => cypher_cmp(a, b).expect("same-rank values compare"),
    }
}

fn eval_pred(g: &Graph, row: &Row, p: &Pred) -> Option<bool> {
    match p {
        Pred::Cmp(t, op, rhs) => {
            let a = eval_term(g, row, t);
            let b = match rhs {
                Operand::Lit(v) => v.clone(),
                Operand::Term(t) => eval_term(g, row, t),
            };
            match *op {
                "=" => cypher_eq(&a, &b),
                "<>" => cypher_eq(&a, &b).map(|e| !e),
                "<" => cypher_cmp(&a, &b).map(|o| o == Ordering::Less),
                "<=" => cypher_cmp(&a, &b).map(|o| o != Ordering::Greater),
                ">" => cypher_cmp(&a, &b).map(|o| o == Ordering::Greater),
                ">=" => cypher_cmp(&a, &b).map(|o| o != Ordering::Less),
                other => unreachable!("operator {other}"),
            }
        }
        Pred::IsNull(t, negated) => Some((eval_term(g, row, t) == Val::Null) != *negated),
        Pred::In(t, xs) => cypher_in(&eval_term(g, row, t), xs),
        Pred::Not(a) => eval_pred(g, row, a).map(|v| !v),
        Pred::And(a, b) => match (eval_pred(g, row, a), eval_pred(g, row, b)) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        Pred::Or(a, b) => match (eval_pred(g, row, a), eval_pred(g, row, b)) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        Pred::Xor(a, b) => match (eval_pred(g, row, a), eval_pred(g, row, b)) {
            (Some(x), Some(y)) => Some(x != y),
            _ => None,
        },
    }
}

/// openCypher `IN`: true if an element equals `x`; otherwise null if any
/// comparison was null; otherwise false. So `x IN []` is false even for a
/// null `x`.
fn cypher_in(x: &Val, xs: &[Val]) -> Option<bool> {
    let mut unknown = false;
    for e in xs {
        match cypher_eq(x, e) {
            Some(true) => return Some(true),
            None => unknown = true,
            Some(false) => {}
        }
    }
    if unknown {
        None
    } else {
        Some(false)
    }
}

/// openCypher equivalence, used for grouping and `DISTINCT`: null equals
/// null, and a number is its value.
fn equivalence_key(v: &Val) -> String {
    match v {
        Val::Null => "null".into(),
        Val::Int(i) => format!("num:{i}"),
        Val::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("num:{}", *f as i64),
        Val::Float(f) => format!("num:{f:?}"),
        Val::Str(s) => format!("str:{s:?}"),
        Val::Bool(b) => format!("bool:{b}"),
        Val::List(xs) => format!(
            "list:[{}]",
            xs.iter().map(equivalence_key).collect::<Vec<_>>().join(",")
        ),
        Val::Node(i) => format!("node:{i}"),
    }
}

/// `sum` over no values. openCypher returns 0; the engine returns null and
/// pins it in `core-query`'s `exec.rs`
/// (`sum_numeric_prop_ignores_null_and_non_numeric`). Documented deviation,
/// ledger row 166, deferred to 0.11 for an owner ruling. Flipping it is this
/// one function.
fn sum_of_nothing() -> Val {
    Val::Null
}

/// openCypher `sum`: an integer over integers, a float once any input is a
/// float.
fn sum_of(xs: &[Val]) -> Val {
    if xs.is_empty() {
        return sum_of_nothing();
    }
    if xs.iter().all(|v| matches!(v, Val::Int(_))) {
        Val::Int(
            xs.iter()
                .map(|v| match v {
                    Val::Int(i) => *i,
                    _ => 0,
                })
                .sum(),
        )
    } else {
        Val::Float(xs.iter().map(|v| as_f64(v).expect("numeric")).sum())
    }
}

fn aggregate(g: &Graph, rows: &[&Row], item: &Item) -> Val {
    let (f, distinct, arg) = match item {
        Item::CountStar => return Val::Int(rows.len() as i64),
        Item::Agg(f, distinct, arg) => (*f, *distinct, arg),
        other => unreachable!("not an aggregate: {other:?}"),
    };
    let mut xs: Vec<Val> = rows
        .iter()
        .map(|r| eval_term(g, r, arg))
        .filter(|v| *v != Val::Null)
        .collect();
    if distinct {
        let mut seen = BTreeSet::new();
        xs.retain(|v| seen.insert(equivalence_key(v)));
    }
    match f {
        AggFn::Count => Val::Int(xs.len() as i64),
        AggFn::Sum => sum_of(&xs),
        AggFn::Avg => {
            if xs.is_empty() {
                Val::Null
            } else {
                let total: f64 = xs.iter().map(|v| as_f64(v).expect("numeric")).sum();
                Val::Float(total / xs.len() as f64)
            }
        }
        AggFn::Min => xs.into_iter().min_by(orderability).unwrap_or(Val::Null),
        AggFn::Max => xs.into_iter().max_by(orderability).unwrap_or(Val::Null),
        AggFn::Collect => Val::List(xs),
    }
}

fn row_key(row: &Row, aliases: &[&String]) -> String {
    aliases
        .iter()
        .map(|a| equivalence_key(&row[*a]))
        .collect::<Vec<_>>()
        .join("|")
}

fn project(g: &Graph, rows: &[Row], p: &Projection) -> Vec<Row> {
    let plain: Vec<&(Item, String, Kind)> = p.items.iter().filter(|i| !i.0.is_agg()).collect();
    let value = |row: &Row, item: &Item| match item {
        Item::Node(v) => row.get(v).cloned().unwrap_or(Val::Null),
        Item::Term(t) => eval_term(g, row, t),
        other => unreachable!("not a plain item: {other:?}"),
    };
    let mut out: Vec<Row> = if plain.len() == p.items.len() {
        rows.iter()
            .map(|r| {
                p.items
                    .iter()
                    .map(|(item, alias, _)| (alias.clone(), value(r, item)))
                    .collect()
            })
            .collect()
    } else {
        let key_aliases: Vec<&String> = plain.iter().map(|i| &i.1).collect();
        let mut index: BTreeMap<String, usize> = BTreeMap::new();
        let mut groups: Vec<(Row, Vec<&Row>)> = Vec::new();
        for r in rows {
            let keys: Row = plain
                .iter()
                .map(|(item, alias, _)| (alias.clone(), value(r, item)))
                .collect();
            let k = row_key(&keys, &key_aliases);
            let at = *index.entry(k).or_insert_with(|| {
                groups.push((keys, Vec::new()));
                groups.len() - 1
            });
            groups[at].1.push(r);
        }
        if plain.is_empty() && groups.is_empty() {
            groups.push((Row::new(), Vec::new()));
        }
        groups
            .into_iter()
            .map(|(mut keys, members)| {
                for (item, alias, _) in p.items.iter().filter(|i| i.0.is_agg()) {
                    keys.insert(alias.clone(), aggregate(g, &members, item));
                }
                keys
            })
            .collect()
    };
    if p.distinct {
        let aliases: Vec<&String> = p.items.iter().map(|i| &i.1).collect();
        let mut seen = BTreeSet::new();
        out.retain(|r| seen.insert(row_key(r, &aliases)));
    }
    out
}

/// The reference answer: every row in order (sorted when the query orders),
/// and the rows `SKIP` and `LIMIT` leave.
struct Answer {
    all: Vec<Vec<Val>>,
    rows: Vec<Vec<Val>>,
}

fn evaluate(g: &Graph, q: &Query) -> Answer {
    let mut rows = match_paths(g, &q.paths);
    if let Some(p) = &q.filter {
        rows.retain(|r| eval_pred(g, r, p) == Some(true));
    }
    if let Some(xs) = &q.unwind {
        rows = rows
            .into_iter()
            .flat_map(|r| {
                xs.iter().map(move |x| {
                    let mut r = r.clone();
                    r.insert("u".into(), x.clone());
                    r
                })
            })
            .collect();
    }
    if let Some((proj, filter)) = &q.with {
        rows = project(g, &rows, proj);
        if let Some(p) = filter {
            rows.retain(|r| eval_pred(g, r, p) == Some(true));
        }
    }
    let projected = project(g, &rows, &q.ret);
    let mut all: Vec<Vec<Val>> = projected
        .iter()
        .map(|r| q.ret.items.iter().map(|i| r[&i.1].clone()).collect())
        .collect();
    if !q.order.is_empty() {
        all.sort_by(|a, b| {
            for (col, desc) in &q.order {
                let o = orderability(&a[*col], &b[*col]);
                let o = if *desc { o.reverse() } else { o };
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        });
    }
    let rows = all
        .iter()
        .skip(q.skip.unwrap_or(0))
        .take(q.limit.unwrap_or(usize::MAX))
        .cloned()
        .collect();
    Answer { all, rows }
}

// ---------------------------------------------------------------------------
// Comparing the engine with the reference
// ---------------------------------------------------------------------------

/// A value as text. A list (only `collect` makes one) is a multiset, since
/// the order `collect` sees its rows in is not defined. `approx` rounds a
/// float to nine significant digits, for `sum` and `avg`.
fn show(v: &Val, approx: bool) -> String {
    match v {
        Val::Float(f) if approx => format!("{f:.8e}f"),
        Val::Float(f) => format!("{f:?}f"),
        Val::List(xs) => {
            let mut parts: Vec<String> = xs.iter().map(|x| show(x, approx)).collect();
            parts.sort();
            format!("[{}]", parts.join(", "))
        }
        other => render_val(other),
    }
}

fn show_rows(q: &Query, rows: &[Vec<Val>]) -> Vec<String> {
    rows.iter()
        .map(|r| {
            let cells: Vec<String> = r
                .iter()
                .zip(&q.ret.items)
                .map(|(v, (item, _, _))| {
                    let approx = matches!(item, Item::Agg(AggFn::Sum | AggFn::Avg, ..));
                    show(v, approx)
                })
                .collect();
            format!("[{}]", cells.join(", "))
        })
        .collect()
}

fn compare(q: &Query, want: &Answer, rs: &ResultSet) -> Result<(), String> {
    let aliases: Vec<String> = q.ret.items.iter().map(|i| i.1.clone()).collect();
    if rs.columns() != aliases.as_slice() {
        return Err(format!("columns {:?}, expected {aliases:?}", rs.columns()));
    }
    let have_rows: Vec<Vec<Val>> = (0..rs.len())
        .map(|i| aliases.iter().map(|a| from_value(rs.get(i, a))).collect())
        .collect();
    let mut have = show_rows(q, &have_rows);
    let mut expected = show_rows(q, &want.rows);
    let ordered = !q.order.is_empty();
    if ordered {
        if have != expected {
            return Err(format!(
                "ordered rows differ\nexpected:\n  {}\nengine:\n  {}",
                expected.join("\n  "),
                have.join("\n  ")
            ));
        }
        return Ok(());
    }
    have.sort();
    expected.sort();
    if q.skip.is_none() && q.limit.is_none() {
        if have != expected {
            return Err(format!(
                "rows differ (as multisets)\nexpected:\n  {}\nengine:\n  {}",
                expected.join("\n  "),
                have.join("\n  ")
            ));
        }
        return Ok(());
    }
    // SKIP / LIMIT without ORDER BY: any rows of the full answer, but the
    // right number of them.
    let mut pool = show_rows(q, &want.all);
    if have.len() != expected.len() {
        return Err(format!(
            "SKIP/LIMIT without ORDER BY returned {} rows, expected {}\nengine:\n  {}",
            have.len(),
            expected.len(),
            have.join("\n  ")
        ));
    }
    for row in &have {
        match pool.iter().position(|r| r == row) {
            Some(at) => {
                pool.swap_remove(at);
            }
            None => {
                return Err(format!(
                    "SKIP/LIMIT returned {row}, which the full answer does not hold (as often)\nfull answer:\n  {}",
                    show_rows(q, &want.all).join("\n  ")
                ))
            }
        }
    }
    Ok(())
}

// Generated shapes that hit a ledgered, deferred defect. The property skips
// them so it keeps checking everything else; each `finding_*` test below pins
// the defect itself. Delete each function, and its call, with its fix.

/// Ledger row 170: values of different types order numbers first in
/// `ORDER BY`, `min` and `max`. Only a value of `Kind::Any` mixes types.
/// Delete with the fix.
fn hits_row_170(q: &Query) -> bool {
    let extreme_of_mixed = |p: &Projection| {
        p.items.iter().any(|(item, _, kind)| {
            *kind == Kind::Any && matches!(item, Item::Agg(AggFn::Min | AggFn::Max, ..))
        })
    };
    let orders_mixed = !q.order.is_empty() && q.ret.items.iter().any(|i| i.2 == Kind::Any);
    orders_mixed
        || extreme_of_mixed(&q.ret)
        || q.with.as_ref().is_some_and(|(p, _)| extreme_of_mixed(p))
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn cases() -> u32 {
    std::env::var("H3_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(48)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]
    #[test]
    fn the_engine_answers_as_the_reference_does(
        nodes in proptest::collection::vec(node_strategy(), 1..=30),
        edges in proptest::collection::vec((any::<u8>(), any::<u8>(), any::<u8>()), 0..45),
        updates in proptest::collection::vec(update_strategy(), 0..5),
        snapshot_slot in any::<u16>(),
        queries in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 3),
    ) {
        let (g, db) = build(&nodes, &edges, &updates, usize::from(snapshot_slot));
        for choices in &queries {
            let q = gen_query(choices);
            if hits_row_170(&q) {
                continue;
            }
            let text = render_query(&q);
            let want = evaluate(&g, &q);
            match db.query(&text, &no_params()) {
                Ok(rs) => {
                    if let Err(why) = compare(&q, &want, &rs) {
                        prop_assert!(false, "{}\n  {}\ngraph:\n  {}", why, text, dump(&g));
                    }
                }
                Err(e) => prop_assert!(false, "the engine refused a generated query:\n  {}\n  {}", text, e),
            }
        }
    }
}

/// The generator must produce queries the engine runs, over a graph where
/// they find rows. A generator whose queries mostly fail, or mostly match
/// nothing, compares nothing.
#[test]
fn generated_queries_run_and_find_rows() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let nodes: Vec<NodeSpec> = (0..24u8)
        .map(|i| NodeSpec {
            label: i,
            props: (0..WRITTEN_PROPS as u8)
                .map(|p| {
                    (i.wrapping_mul(7).wrapping_add(p) % 5 != 0)
                        .then_some(i.wrapping_mul(31).wrapping_add(p * 3))
                })
                .collect(),
        })
        .collect();
    let edges: Vec<(u8, u8, u8)> = (0..90u8)
        .map(|i| (i / 3, i.wrapping_mul(5), i.wrapping_mul(11).wrapping_add(3)))
        .collect();
    let (g, db) = build(&nodes, &edges, &[], 30);
    let total = 200;
    let mut ran = 0;
    let mut with_rows = 0;
    let mut failures = Vec::new();
    for _ in 0..total {
        let choices: Vec<u8> = (0..64).map(|_| next() as u8).collect();
        let q = gen_query(&choices);
        let text = render_query(&q);
        match db.query(&text, &no_params()) {
            Ok(rs) => {
                ran += 1;
                if !rs.is_empty() {
                    with_rows += 1;
                }
            }
            Err(e) => failures.push(format!("{text}\n    {e}")),
        }
        if q.ret.items.iter().all(|i| i.0.is_agg()) {
            assert_eq!(
                evaluate(&g, &q).all.len(),
                1,
                "an aggregate with no grouping key is one row: {text}"
            );
        }
    }
    assert!(
        ran * 10 >= total * 9,
        "only {ran} of {total} generated queries ran:\n{}",
        failures.join("\n")
    );
    assert!(
        with_rows * 2 >= total,
        "only {with_rows} of {total} generated queries returned rows"
    );
}

/// The reference's own semantics on cases whose openCypher answer is known
/// (the TCK and the openCypher comparability rules). A reference that is wrong
/// here would report engine bugs that are not there, or miss real ones.
#[test]
fn the_reference_follows_opencypher_on_known_cases() {
    let s = |x: &str| Val::Str(x.into());
    // Comparability and equality.
    assert_eq!(cypher_eq(&Val::Int(1), &Val::Float(1.0)), Some(true));
    assert_eq!(cypher_eq(&Val::Int(1), &s("1")), Some(false));
    assert_eq!(cypher_eq(&Val::Null, &Val::Null), None);
    assert_eq!(cypher_cmp(&Val::Int(1), &s("a")), None);
    assert_eq!(
        cypher_cmp(&Val::Bool(false), &Val::Bool(true)),
        Some(Ordering::Less)
    );
    assert_eq!(cypher_cmp(&Val::Bool(true), &Val::Int(1)), None);
    // IN.
    assert_eq!(cypher_in(&Val::Int(1), &[Val::Int(2), Val::Null]), None);
    assert_eq!(
        cypher_in(&Val::Int(2), &[Val::Int(2), Val::Null]),
        Some(true)
    );
    assert_eq!(cypher_in(&Val::Null, &[]), Some(false));
    assert_eq!(cypher_in(&Val::Null, &[Val::Int(1)]), None);
    // min / max over mixed values (TCK Aggregation: max is 1, min is [1, 2]).
    let mixed = [
        Val::Int(1),
        s("a"),
        Val::List(vec![Val::Int(1), Val::Int(2)]),
        Val::Float(0.2),
        s("b"),
    ];
    assert_eq!(
        mixed.iter().cloned().max_by(orderability),
        Some(Val::Int(1))
    );
    assert_eq!(
        mixed.iter().cloned().min_by(orderability),
        Some(Val::List(vec![Val::Int(1), Val::Int(2)]))
    );
    // sum keeps integers integral and turns float on a float.
    assert_eq!(sum_of(&[Val::Int(10), Val::Int(20)]), Val::Int(30));
    assert_eq!(sum_of(&[Val::Int(1), Val::Float(0.5)]), Val::Float(1.5));
    // Three-valued connectives, through a whole query over a one-node graph.
    let g = Graph {
        nodes: vec![GNode {
            label: 0,
            key: "n00".into(),
            props: BTreeMap::from([("i", Val::Int(1))]),
        }],
        edges: Vec::new(),
    };
    let row = Row::from([("a".to_string(), Val::Node(0))]);
    let null_cmp = || {
        Box::new(Pred::Cmp(
            Term::Prop("a".into(), "z"),
            "=",
            Operand::Lit(Val::Int(1)),
        ))
    };
    let t = || {
        Box::new(Pred::Cmp(
            Term::Prop("a".into(), "i"),
            "=",
            Operand::Lit(Val::Int(1)),
        ))
    };
    let f = || {
        Box::new(Pred::Cmp(
            Term::Prop("a".into(), "i"),
            "=",
            Operand::Lit(Val::Int(2)),
        ))
    };
    assert_eq!(
        eval_pred(&g, &row, &Pred::And(null_cmp(), f())),
        Some(false)
    );
    assert_eq!(eval_pred(&g, &row, &Pred::Or(null_cmp(), t())), Some(true));
    assert_eq!(eval_pred(&g, &row, &Pred::Xor(null_cmp(), t())), None);
    assert_eq!(eval_pred(&g, &row, &Pred::Not(null_cmp())), None);
    // An aggregate with no grouping key over no rows is one row; sum is the
    // engine's documented null (row 166), count is 0.
    let q = Query {
        paths: vec![Path {
            nodes: vec![("a".into(), Some(1))],
            rels: Vec::new(),
            bare_rels: Vec::new(),
        }],
        filter: None,
        unwind: None,
        with: None,
        ret: Projection {
            distinct: false,
            items: vec![
                (Item::CountStar, "c0".into(), Kind::Int),
                (
                    Item::Agg(AggFn::Sum, false, Term::Prop("a".into(), "i")),
                    "c1".into(),
                    Kind::Int,
                ),
            ],
        },
        order: Vec::new(),
        skip: None,
        limit: None,
    };
    assert_eq!(evaluate(&g, &q).rows, vec![vec![Val::Int(0), Val::Null]]);
}

// ---------------------------------------------------------------------------
// Disagreements found when this test was written, as shrunk regressions.
// Each is an engine answer openCypher does not give and the docs do not
// describe. They fail until the engine is fixed or the deviation is ruled on
// and documented (then it becomes one named function, like `sum_of_nothing`).
// ---------------------------------------------------------------------------

fn fixed_db(nodes: &[(&str, Vec<(&str, Value)>)]) -> GraphDb<SimFs> {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();
    for (i, (label, props)) in nodes.iter().enumerate() {
        let props = props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        db.insert_node(label, &key_of(i), props).unwrap();
    }
    db
}

fn rows_of(db: &GraphDb<SimFs>, q: &str) -> (Vec<String>, Vec<Vec<Option<Value>>>) {
    let rs = db
        .query(q, &no_params())
        .unwrap_or_else(|e| panic!("{q}\n{e}"));
    let rows = (0..rs.len()).map(|i| rs.row(i).to_vec()).collect();
    (rs.columns().to_vec(), rows)
}

/// `SKIP` and `LIMIT` do not apply to an aggregate with no grouping key: the
/// one row it makes is returned under `SKIP 1` and `LIMIT 0`. openCypher
/// applies them to that row like any other, so both return no rows.
#[test]
fn finding_skip_and_limit_apply_to_an_ungrouped_aggregate() {
    let db = fixed_db(&[("A", vec![])]);
    for q in [
        "MATCH (a) RETURN count(*) AS c SKIP 1",
        "MATCH (a) RETURN count(*) AS c LIMIT 0",
    ] {
        assert_eq!(rows_of(&db, q).1, Vec::<Vec<Option<Value>>>::new(), "{q}");
    }
}

/// A `RETURN` with an aggregate, straight after an aggregating `WITH` (no
/// `WHERE` between them), is dropped: the query returns the `WITH`'s columns
/// and rows. With a `WHERE` in between the `RETURN` runs. openCypher
/// aggregates the `WITH`'s rows again.
#[test]
fn finding_an_aggregating_return_after_an_aggregating_with_runs() {
    let s = |x: &str| vec![("s", Value::Str(x.into()))];
    let db = fixed_db(&[("A", s("a")), ("A", s("a")), ("B", s("b"))]);
    let (cols, rows) = rows_of(
        &db,
        "MATCH (a) WITH a.s AS w0, count(*) AS w1 RETURN count(*) AS c0",
    );
    assert_eq!(cols, vec!["c0".to_string()]);
    assert_eq!(rows, vec![vec![Some(Value::Int(2))]]);
    let (cols, rows) = rows_of(
        &db,
        "MATCH (a) WITH a.s AS w0, count(*) AS w1 RETURN sum(w1) AS c0",
    );
    assert_eq!(cols, vec!["c0".to_string()]);
    assert_eq!(rows, vec![vec![Some(Value::Int(3))]]);
}

/// Values of different types order numbers, strings, booleans, lists, null in
/// `ORDER BY`, and `min` / `max` follow that order. openCypher orders lists,
/// strings, booleans, numbers, null (the TCK's `max` over mixed values is the
/// number, `min` the list). query.md says only that null sorts last.
#[test]
#[ignore = "ledger row 170, fixed in 0.10.1: un-ignore with the fix"]
fn finding_values_of_different_types_order_as_opencypher_orders_them() {
    let m = |v: Value| vec![("m", v)];
    let db = fixed_db(&[
        ("A", m(Value::Int(1))),
        ("A", m(Value::Str("a".into()))),
        ("A", m(Value::Bool(true))),
    ]);
    let (_, rows) = rows_of(&db, "MATCH (a) RETURN a.m AS m ORDER BY m");
    assert_eq!(
        rows,
        vec![
            vec![Some(Value::Str("a".into()))],
            vec![Some(Value::Bool(true))],
            vec![Some(Value::Int(1))],
        ]
    );
    let (_, rows) = rows_of(&db, "MATCH (a) RETURN min(a.m) AS lo, max(a.m) AS hi");
    assert_eq!(
        rows,
        vec![vec![Some(Value::Str("a".into())), Some(Value::Int(1))]]
    );
}
