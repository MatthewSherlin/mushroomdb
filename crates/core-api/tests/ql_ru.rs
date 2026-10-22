//! QL-RU (defect-ledger row 134): within one MATCH clause no relationship is
//! bound twice, across fixed and variable-length hops and across the clause's
//! comma-separated parts; separate clauses (an OPTIONAL MATCH included) do not
//! share the constraint. Checked through the public API: reads, writes driven
//! by such a MATCH, every way a store is built or read, and a randomized
//! differential against a reference matcher written here.
//!
//! On v0.9.2, over `a -R-> b -R-> c -R-> d`,
//! `MATCH (s {id: 'b'})<-[*1..2]-(x)-[:R]->(y) DETACH DELETE y` deleted `b`:
//! the walk back from `b` and the fixed hop forward from `a` used the same
//! edge. The correct match is empty, so nothing is deleted.

use mushroomdb::{GraphDb, SharedDb, Value};
use std::collections::{BTreeMap, BTreeSet};

type Db = GraphDb<core_storage::fs::RealFs>;
type Rows = Vec<Vec<Option<Value>>>;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-qlru-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn s(v: &str) -> Option<Value> {
    Some(Value::Str(v.into()))
}

fn rows_with(db: &Db, q: &str, params: &BTreeMap<String, Value>) -> Result<Rows, String> {
    let rs = db.query(q, params).map_err(|e| format!("{q}: {e}"))?;
    Ok((0..rs.len()).map(|i| rs.row(i).to_vec()).collect())
}

fn rows(db: &Db, q: &str) -> Rows {
    rows_with(db, q, &BTreeMap::new()).unwrap_or_else(|e| panic!("{e}"))
}

fn sorted(mut v: Rows) -> Rows {
    v.sort_by_key(|r| format!("{r:?}"));
    v
}

fn keys(db: &Db, q: &str) -> Vec<String> {
    rows(db, q)
        .into_iter()
        .map(|r| match &r[0] {
            Some(Value::Str(k)) => k.clone(),
            other => panic!("{q}: not a key: {other:?}"),
        })
        .collect()
}

const CHAIN_NODES: [(&str, bool); 4] = [("a", true), ("b", true), ("c", false), ("d", true)];
const CHAIN_EDGES: [(&str, &str); 3] = [("a", "b"), ("b", "c"), ("c", "d")];

/// `a -R-> b -R-> c -R-> d`, all `Person`; `c` is the inactive one.
fn chain(dir: &std::path::Path) -> Db {
    let mut db = GraphDb::open(dir).unwrap();
    let mut batch = db.batch();
    for (key, active) in CHAIN_NODES {
        batch.insert_node(
            "Person",
            key,
            vec![("active".to_owned(), Value::Bool(active))],
        );
    }
    for (src, dst) in CHAIN_EDGES {
        batch.insert_edge("R", src, dst);
    }
    batch.commit().unwrap();
    db
}

fn chain_many_commits(dir: &std::path::Path) -> Db {
    let mut db = GraphDb::open(dir).unwrap();
    for (key, active) in CHAIN_NODES {
        db.insert_node(
            "Person",
            key,
            vec![("active".to_owned(), Value::Bool(active))],
        )
        .unwrap();
    }
    for (src, dst) in CHAIN_EDGES {
        db.insert_edge("R", src, dst).unwrap();
    }
    db
}

fn write(db: &mut Db, q: &str) {
    db.query_write(q, &BTreeMap::new())
        .unwrap_or_else(|e| panic!("{q}: {e}"));
}

#[test]
fn writes_driven_by_a_pattern_touch_only_its_unique_matches() {
    let mut db = chain(&tmp("writes"));
    // Every match would reuse a->b; nothing may be deleted.
    write(
        &mut db,
        "MATCH (s {id: 'b'})<-[*1..2]-(x)-[:R]->(y) DETACH DELETE y",
    );
    assert_eq!(
        keys(&db, "MATCH (n) RETURN n ORDER BY n"),
        ["a", "b", "c", "d"]
    );
    // Each walk back one edge is followed by the same edge forward: no match.
    write(&mut db, "MATCH (a)<-[*1..1]-(b)-[:R]->(c) SET c.hit = true");
    assert!(keys(&db, "MATCH (n) WHERE n.hit = true RETURN n").is_empty());
    // A walk forward then the next edge along does match.
    write(
        &mut db,
        "MATCH (s {id: 'a'})-[*1..2]->(x)-[:R]->(y) SET y.next = true",
    );
    assert_eq!(
        keys(&db, "MATCH (n) WHERE n.next = true RETURN n ORDER BY n"),
        ["c", "d"]
    );
    // Separate clauses may share an edge; comma-separated parts may not.
    write(
        &mut db,
        "MATCH (a)-[r]->(b) MATCH (c)-[s]->(b) SET c.clauses = true",
    );
    assert_eq!(
        keys(&db, "MATCH (n) WHERE n.clauses = true RETURN n ORDER BY n"),
        ["a", "b", "c"]
    );
    write(
        &mut db,
        "MATCH (a)-[r]->(b), (c)-[s]->(b) SET c.commas = true",
    );
    assert!(keys(&db, "MATCH (n) WHERE n.commas = true RETURN n").is_empty());
    // The walk may not reuse the fixed hop's edge either.
    write(
        &mut db,
        "MATCH (x {id: 'b'})-[r]->(y), (x)-[*1..3]-(z) SET z.reach = true",
    );
    assert_eq!(
        keys(&db, "MATCH (n) WHERE n.reach = true RETURN n ORDER BY n"),
        ["a"]
    );
}

#[test]
fn a_transaction_write_sees_the_same_matches() {
    let dir = tmp("tx");
    drop(chain(&dir));
    let shared = SharedDb::open(&dir).unwrap();
    let mut tx = shared.begin_transaction();
    tx.execute(
        "MATCH (s {id: 'b'})<-[*1..2]-(x)-[:R]->(y) DETACH DELETE y",
        &BTreeMap::new(),
    )
    .unwrap();
    tx.execute(
        "MATCH (a)<-[*1..1]-(b)-[:R]->(c) SET c.hit = true",
        &BTreeMap::new(),
    )
    .unwrap();
    tx.execute(
        "MATCH (a)-[r]->(b) MATCH (c)-[s]->(b) SET c.clauses = true",
        &BTreeMap::new(),
    )
    .unwrap();
    tx.commit().unwrap();
    let db = shared.read();
    let db = &*db;
    assert_eq!(
        keys(db, "MATCH (n) RETURN n ORDER BY n"),
        ["a", "b", "c", "d"]
    );
    assert!(keys(db, "MATCH (n) WHERE n.hit = true RETURN n").is_empty());
    assert_eq!(
        keys(db, "MATCH (n) WHERE n.clauses = true RETURN n ORDER BY n"),
        ["a", "b", "c"]
    );
}

const QUERIES: [&str; 7] = [
    "MATCH (s {id: 'd'})<-[*1..3]-(x)-[:R]->(y) RETURN x, y",
    "MATCH (s {id: 'a'})-[*1..2]->(x)-[:R]->(y) RETURN x, y ORDER BY x",
    "MATCH (a)-[r]->(b), (c)-[s]->(b) RETURN a, c",
    "MATCH (a)-[r]->(b) MATCH (c)-[s]->(b) RETURN a, c ORDER BY a",
    "MATCH (a {id: 'd'}) \
     OPTIONAL MATCH (a)<-[*1..3]-(b:Person {active: true})-[:R]->(c:Person) RETURN a, b, c",
    "MATCH (a {id: 'a'})-[r]->(b) OPTIONAL MATCH (b)<-[s]-(x) RETURN x",
    "MATCH p = (s {id: 'b'})-[*1..3]-(x)-[:R]-(y) RETURN p.nodes AS n ORDER BY n",
];

fn list(xs: &[&str]) -> Option<Value> {
    Some(Value::List(
        xs.iter().map(|x| Value::Str((*x).into())).collect(),
    ))
}

fn expected() -> Vec<Rows> {
    vec![
        vec![],
        vec![vec![s("b"), s("c")], vec![s("c"), s("d")]],
        vec![],
        vec![
            vec![s("a"), s("a")],
            vec![s("b"), s("b")],
            vec![s("c"), s("c")],
        ],
        vec![vec![s("d"), None, None]],
        vec![vec![s("a")]],
        // Every other walk out of b leaves no unused edge for the last hop.
        vec![vec![list(&["b", "c", "d"])]],
    ]
}

fn answers(db: &Db) -> Vec<Rows> {
    QUERIES.iter().map(|q| rows(db, q)).collect()
}

#[test]
fn uniqueness_holds_live_replayed_snapshotted_reopened_and_indexed() {
    let want = expected();
    let dir = tmp("states");
    let db = chain(&dir);
    assert_eq!(answers(&db), want, "live, one batch");
    drop(db);
    let replayed = GraphDb::open(&dir).unwrap();
    assert_eq!(answers(&replayed), want, "WAL replay");
    let mut snapshotting = replayed;
    snapshotting.snapshot().unwrap();
    assert_eq!(answers(&snapshotting), want, "snapshotting");
    drop(snapshotting);
    let mut reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(answers(&reopened), want, "reopened from a snapshot");
    reopened.enable_index("Person", "active").unwrap();
    assert_eq!(answers(&reopened), want, "indexed");
    let many = chain_many_commits(&tmp("many"));
    assert_eq!(answers(&many), want, "many commits");
}

// ---------------------------------------------------------------------------
// Randomized differential: random graphs (self-loops in about half of them)
// and random patterns in every hop form, against a reference matcher that
// shares no code with the engine. Ported from the QL-VL review's external
// harness and extended to cross-hop uniqueness. `QLRU_GRAPHS` raises the
// graph count (default 120).
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len())]
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

struct RefNode {
    key: String,
    label: &'static str,
    active: Option<bool>,
}

struct RefGraph {
    nodes: Vec<RefNode>,
    /// `(type, src, dst)`; at most one edge per triple, as in the store.
    edges: Vec<(&'static str, usize, usize)>,
}

fn random_graph(rng: &mut Rng) -> RefGraph {
    let n = 2 + rng.below(6);
    let loops = rng.chance(50);
    let nodes = (0..n)
        .map(|i| RefNode {
            key: format!("n{i}"),
            label: rng.pick(&["A", "B"]),
            active: rng.pick(&[Some(true), Some(false), None]),
        })
        .collect();
    let mut edges = Vec::new();
    for _ in 0..rng.below(n * 3 / 2 + 1) {
        let edge = (rng.pick(&["X", "Y"]), rng.below(n), rng.below(n));
        if (edge.1 != edge.2 || loops) && !edges.contains(&edge) {
            edges.push(edge);
        }
    }
    RefGraph { nodes, edges }
}

fn load(g: &RefGraph, dir: &std::path::Path, indexed: bool) -> Db {
    let mut db = GraphDb::open(dir).unwrap();
    if indexed {
        db.enable_index("A", "active").unwrap();
        db.enable_index("B", "active").unwrap();
    }
    let mut batch = db.batch();
    for n in &g.nodes {
        let props = n
            .active
            .map(|a| vec![("active".to_owned(), Value::Bool(a))])
            .unwrap_or_default();
        batch.insert_node(n.label, &n.key, props);
    }
    for &(t, src, dst) in &g.edges {
        batch.insert_edge(t, &g.nodes[src].key, &g.nodes[dst].key);
    }
    batch.commit().unwrap();
    db
}

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Right,
    Left,
    Both,
}

/// A hop's length: one fixed relationship, or a variable-length form.
#[derive(Clone, Copy)]
enum Len {
    Fixed,
    Star,
    Exact(usize),
    Range(usize, usize),
    UpTo(usize),
}

impl Len {
    fn bounds(self) -> (usize, usize) {
        match self {
            Len::Fixed => (1, 1),
            Len::Star => (1, 10),
            Len::Exact(n) => (n, n),
            Len::Range(a, b) => (a, b),
            Len::UpTo(m) => (1, m),
        }
    }

    fn text(self) -> String {
        match self {
            Len::Fixed => String::new(),
            Len::Star => "*".into(),
            Len::Exact(n) => format!("*{n}"),
            Len::Range(a, b) => format!("*{a}..{b}"),
            Len::UpTo(m) => format!("*..{m}"),
        }
    }
}

#[derive(Clone, Copy)]
struct Hop {
    types: &'static [&'static str],
    dir: Dir,
    len: Len,
}

impl Hop {
    fn text(self) -> String {
        let types = if self.types.is_empty() {
            String::new()
        } else {
            format!(":{}", self.types.join("|"))
        };
        let inner = format!("[{types}{}]", self.len.text());
        match self.dir {
            Dir::Right => format!("-{inner}->"),
            Dir::Left => format!("<-{inner}-"),
            Dir::Both => format!("-{inner}-"),
        }
    }

    /// The node this hop reaches over edge `i` from `at`, if it may take it.
    /// An undirected self-loop is one relationship, taken once.
    fn step(self, g: &RefGraph, i: usize, at: usize) -> Option<usize> {
        let (t, src, dst) = g.edges[i];
        if !self.types.is_empty() && !self.types.contains(&t) {
            return None;
        }
        match self.dir {
            Dir::Right if src == at => Some(dst),
            Dir::Left if dst == at => Some(src),
            Dir::Both if src == at => Some(dst),
            Dir::Both if dst == at => Some(src),
            _ => None,
        }
    }
}

/// A node in a pattern: its variable and inline filter. `param` is
/// `{id: $id}`, resolved against the query's parameter.
#[derive(Clone)]
struct NodeP {
    var: String,
    label: Option<&'static str>,
    active: Option<bool>,
    param: bool,
}

impl NodeP {
    fn plain(var: &str) -> Self {
        NodeP {
            var: var.into(),
            label: None,
            active: None,
            param: false,
        }
    }

    fn text(&self) -> String {
        let label = self.label.map(|l| format!(":{l}")).unwrap_or_default();
        let mut props = Vec::new();
        if let Some(a) = self.active {
            props.push(format!("active: {a}"));
        }
        if self.param {
            props.push("id: $id".to_owned());
        }
        let props = if props.is_empty() {
            String::new()
        } else {
            format!(" {{{}}}", props.join(", "))
        };
        format!("({}{label}{props})", self.var)
    }

    fn ok(&self, g: &RefGraph, n: usize, id: usize) -> bool {
        let node = &g.nodes[n];
        self.label.is_none_or(|l| node.label == l)
            && self.active.is_none_or(|a| node.active == Some(a))
            && (!self.param || n == id)
    }
}

#[derive(Clone)]
struct Part {
    path: Option<String>,
    start: NodeP,
    hops: Vec<(Hop, NodeP)>,
}

impl Part {
    fn text(&self) -> String {
        let mut out = self
            .path
            .as_ref()
            .map(|p| format!("{p} = "))
            .unwrap_or_default();
        out.push_str(&self.start.text());
        for (hop, node) in &self.hops {
            out.push_str(&hop.text());
            out.push_str(&node.text());
        }
        out
    }
}

struct Clause {
    optional: bool,
    parts: Vec<Part>,
}

#[derive(Clone, Debug, PartialEq)]
enum Bind {
    Node(usize),
    Path(Vec<usize>, usize),
    Null,
}

type RefRow = BTreeMap<String, Bind>;

struct Matcher<'g> {
    g: &'g RefGraph,
    id: usize,
}

impl Matcher<'_> {
    /// Bind `node` to `n`, or check `n` against the binding it already has.
    fn bind(&self, row: &mut RefRow, node: &NodeP, n: usize) -> bool {
        if !node.ok(self.g, n, self.id) {
            return false;
        }
        match row.get(&node.var) {
            Some(Bind::Node(m)) => *m == n,
            Some(_) => false,
            None => {
                row.insert(node.var.clone(), Bind::Node(n));
                true
            }
        }
    }

    /// Every way to match `parts[k..]` given `row`, with `used` the edges
    /// the clause has bound so far.
    fn parts(&self, parts: &[Part], row: &RefRow, used: &mut Vec<usize>, out: &mut Vec<RefRow>) {
        let Some(part) = parts.first() else {
            out.push(row.clone());
            return;
        };
        for n in 0..self.g.nodes.len() {
            let mut next = row.clone();
            if self.bind(&mut next, &part.start, n) {
                let mut nodes = vec![n];
                self.hops(parts, 0, &next, n, &mut nodes, used, out);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn hops(
        &self,
        parts: &[Part],
        h: usize,
        row: &RefRow,
        at: usize,
        nodes: &mut Vec<usize>,
        used: &mut Vec<usize>,
        out: &mut Vec<RefRow>,
    ) {
        let part = &parts[0];
        let Some((hop, dest)) = part.hops.get(h) else {
            let mut next = row.clone();
            if let Some(p) = &part.path {
                next.insert(p.clone(), Bind::Path(nodes.clone(), nodes.len() - 1));
            }
            self.parts(&parts[1..], &next, used, out);
            return;
        };
        let (min, max) = hop.len.bounds();
        self.walk(
            parts,
            h,
            row,
            at,
            0,
            (min, max),
            *hop,
            dest,
            nodes,
            used,
            out,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        parts: &[Part],
        h: usize,
        row: &RefRow,
        at: usize,
        depth: usize,
        (min, max): (usize, usize),
        hop: Hop,
        dest: &NodeP,
        nodes: &mut Vec<usize>,
        used: &mut Vec<usize>,
        out: &mut Vec<RefRow>,
    ) {
        if depth >= min && depth >= 1 {
            let mut next = row.clone();
            if self.bind(&mut next, dest, at) {
                self.hops(parts, h + 1, &next, at, nodes, used, out);
            }
        }
        if depth == max {
            return;
        }
        for i in 0..self.g.edges.len() {
            if used.contains(&i) {
                continue;
            }
            let Some(to) = hop.step(self.g, i, at) else {
                continue;
            };
            used.push(i);
            nodes.push(to);
            self.walk(
                parts,
                h,
                row,
                to,
                depth + 1,
                (min, max),
                hop,
                dest,
                nodes,
                used,
                out,
            );
            nodes.pop();
            used.pop();
        }
    }

    fn clause(&self, clause: &Clause, rows: Vec<RefRow>) -> Vec<RefRow> {
        let mut out = Vec::new();
        for row in rows {
            let mut found = Vec::new();
            self.parts(&clause.parts, &row, &mut Vec::new(), &mut found);
            if found.is_empty() && clause.optional {
                let mut nulls = row.clone();
                for part in &clause.parts {
                    let vars = std::iter::once(&part.start.var)
                        .chain(part.hops.iter().map(|(_, n)| &n.var))
                        .chain(&part.path);
                    for v in vars {
                        nulls.entry(v.clone()).or_insert(Bind::Null);
                    }
                }
                out.push(nulls);
            }
            out.extend(found);
        }
        out
    }
}

struct Case {
    shape: &'static str,
    clauses: Vec<Clause>,
    /// Columns: node variables, then each path's `nodes` and `length`.
    nodes: Vec<String>,
    paths: Vec<String>,
}

impl Case {
    fn match_text(&self) -> String {
        self.clauses
            .iter()
            .map(|c| {
                let kw = if c.optional {
                    "OPTIONAL MATCH"
                } else {
                    "MATCH"
                };
                let parts: Vec<String> = c.parts.iter().map(Part::text).collect();
                format!("{kw} {}", parts.join(", "))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn cypher(&self) -> String {
        let mut cols: Vec<String> = self.nodes.clone();
        for p in &self.paths {
            cols.push(format!("{p}.nodes AS {p}_nodes"));
            cols.push(format!("{p}.length AS {p}_len"));
        }
        format!("{} RETURN {}", self.match_text(), cols.join(", "))
    }

    fn expected(&self, g: &RefGraph, id: usize) -> Rows {
        let m = Matcher { g, id };
        let mut rows = vec![RefRow::new()];
        for clause in &self.clauses {
            rows = m.clause(clause, rows);
        }
        let key = |n: usize| Value::Str(g.nodes[n].key.clone());
        let out = rows
            .iter()
            .map(|row| {
                let mut cells: Vec<Option<Value>> = self
                    .nodes
                    .iter()
                    .map(|v| match row.get(v) {
                        Some(Bind::Node(n)) => Some(key(*n)),
                        _ => None,
                    })
                    .collect();
                for p in &self.paths {
                    match row.get(p) {
                        Some(Bind::Path(nodes, len)) => {
                            cells.push(Some(Value::List(nodes.iter().map(|n| key(*n)).collect())));
                            cells.push(Some(Value::Int(*len as i64)));
                        }
                        _ => cells.extend([None, None]),
                    }
                }
                cells
            })
            .collect();
        sorted(out)
    }
}

fn random_len(rng: &mut Rng, allow_star: bool) -> Len {
    match rng.below(if allow_star { 5 } else { 4 }) {
        0 => Len::Exact(1 + rng.below(3)),
        1 => {
            let a = 1 + rng.below(2);
            Len::Range(a, a + rng.below(3))
        }
        2 => Len::UpTo(1 + rng.below(3)),
        3 => Len::Exact(1),
        _ => Len::Star,
    }
}

fn random_node(rng: &mut Rng, var: &str) -> NodeP {
    NodeP {
        var: var.into(),
        label: if rng.chance(25) {
            Some(rng.pick(&["A", "B"]))
        } else {
            None
        },
        active: if rng.chance(20) {
            Some(rng.pick(&[true, false]))
        } else {
            None
        },
        param: false,
    }
}

/// `count` hops from `start`, at most two of them variable-length and an
/// unbounded `*` only when it is the only variable-length hop of the case.
fn random_hops(
    rng: &mut Rng,
    vars: &mut Vec<String>,
    count: usize,
    varlen: &mut usize,
) -> Vec<(Hop, NodeP)> {
    let types: [&'static [&'static str]; 4] = [&[], &["X"], &["Y"], &["X", "Y"]];
    (0..count)
        .map(|_| {
            let len = if *varlen < 2 && rng.chance(55) {
                *varlen += 1;
                let allow_star = *varlen == 1 && rng.chance(30);
                random_len(rng, allow_star)
            } else {
                Len::Fixed
            };
            let hop = Hop {
                types: rng.pick(&types),
                dir: rng.pick(&[Dir::Right, Dir::Left, Dir::Both]),
                len,
            };
            let var = format!("v{}", vars.len());
            vars.push(var.clone());
            (hop, random_node(rng, &var))
        })
        .collect()
}

/// One random case of the given shape.
fn random_case(rng: &mut Rng, shape: &'static str) -> Case {
    let mut vars = vec!["v0".to_owned()];
    let mut varlen = 0;
    let mut paths = Vec::new();
    let path = |rng: &mut Rng, paths: &mut Vec<String>| {
        rng.chance(35).then(|| {
            let p = format!("p{}", paths.len());
            paths.push(p.clone());
            p
        })
    };
    let first_hops = 1 + rng.below(if shape == "chain" { 3 } else { 2 });
    let first = Part {
        path: path(rng, &mut paths),
        start: random_node(rng, "v0"),
        hops: random_hops(rng, &mut vars, first_hops, &mut varlen),
    };
    let clauses = match shape {
        "chain" => vec![Clause {
            optional: false,
            parts: vec![first],
        }],
        "commas" | "clauses" => {
            // The second part starts at a node of the first or a new one, and
            // may end at a node of the first.
            let shared = vars.clone();
            let start = if rng.chance(60) {
                NodeP::plain(&shared[rng.below(shared.len())])
            } else {
                let v = format!("v{}", vars.len());
                vars.push(v.clone());
                random_node(rng, &v)
            };
            let count = 1 + rng.below(2);
            let mut hops = random_hops(rng, &mut vars, count, &mut varlen);
            if rng.chance(40) {
                let end = &shared[rng.below(shared.len())];
                vars.pop();
                hops.last_mut().unwrap().1.var = end.clone();
            }
            let second = Part {
                path: path(rng, &mut paths),
                start,
                hops,
            };
            if shape == "commas" {
                vec![Clause {
                    optional: false,
                    parts: vec![first, second],
                }]
            } else {
                vec![
                    Clause {
                        optional: false,
                        parts: vec![first],
                    },
                    Clause {
                        optional: false,
                        parts: vec![second],
                    },
                ]
            }
        }
        _ => {
            // OPTIONAL MATCH from a bound start, after a plain scan.
            let mut part = first;
            let start = std::mem::replace(&mut part.start, NodeP::plain("v0"));
            vec![
                Clause {
                    optional: false,
                    parts: vec![Part {
                        path: None,
                        start,
                        hops: vec![],
                    }],
                },
                Clause {
                    optional: true,
                    parts: vec![part],
                },
            ]
        }
    };
    let mut nodes: Vec<String> = Vec::new();
    for c in &clauses {
        for p in &c.parts {
            for v in std::iter::once(&p.start.var).chain(p.hops.iter().map(|(_, n)| &n.var)) {
                if !nodes.contains(v) {
                    nodes.push(v.clone());
                }
            }
        }
    }
    Case {
        shape,
        clauses,
        nodes,
        paths,
    }
}

/// Put `{id: $id}` on one node of the case (its first occurrence).
fn add_param(rng: &mut Rng, case: &mut Case) {
    let var = case.nodes[rng.below(case.nodes.len())].clone();
    for c in &mut case.clauses {
        for p in &mut c.parts {
            for node in std::iter::once(&mut p.start).chain(p.hops.iter_mut().map(|(_, n)| n)) {
                if node.var == var {
                    node.param = true;
                    return;
                }
            }
        }
    }
}

fn engine_rows(db: &Db, q: &str, params: &BTreeMap<String, Value>) -> Result<Rows, String> {
    rows_with(db, q, params).map(sorted)
}

#[test]
fn randomized_patterns_match_a_reference_matcher() {
    let graphs: usize = std::env::var("QLRU_GRAPHS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let mut rng = Rng(0x5EED_0134_D1FF_0001);
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();
    let base = tmp("random");
    for gi in 0..graphs {
        let g = random_graph(&mut rng);
        let mut db = load(&g, &base.join(format!("g{gi}")), false);
        let idb = load(&g, &base.join(format!("g{gi}i")), true);
        for qi in 0..8 {
            let shape = rng.pick(&["chain", "commas", "clauses", "optional"]);
            let mut case = random_case(&mut rng, shape);
            let id = rng.below(g.nodes.len());
            let param = rng.chance(25);
            if param {
                add_param(&mut rng, &mut case);
            }
            let params = if param {
                BTreeMap::from([("id".to_owned(), Value::Str(g.nodes[id].key.clone()))])
            } else {
                BTreeMap::new()
            };
            let q = case.cypher();
            let want = case.expected(&g, id);
            let got = engine_rows(&db, &q, &params);
            *counts.entry(case.shape).or_default() += 1;
            if got.as_ref() != Ok(&want) {
                failures.push(format!(
                    "[{}] {q}\n  edges {:?}\n  got  {got:?}\n  want {want:?}",
                    case.shape, g.edges
                ));
            }
            if engine_rows(&idb, &q, &params) != got {
                failures.push(format!("[indexed] {q}: the indexed store differs"));
            }
            // A write driven by the same MATCH lands on exactly the matched
            // nodes (writes take no OPTIONAL MATCH or path names here).
            if case.paths.is_empty() && !case.clauses.iter().any(|c| c.optional) {
                let target = case.nodes.last().unwrap().clone();
                let prop = format!("hit_{qi}");
                let w = format!("{} SET {target}.{prop} = true", case.match_text());
                match db.query_write(&w, &params) {
                    Ok(_) => {
                        let col = case.nodes.iter().position(|v| *v == target).unwrap();
                        let want: BTreeSet<String> = want
                            .iter()
                            .filter_map(|r| match &r[col] {
                                Some(Value::Str(k)) => Some(k.clone()),
                                _ => None,
                            })
                            .collect();
                        let got: BTreeSet<String> =
                            keys(&db, &format!("MATCH (n) WHERE n.{prop} = true RETURN n"))
                                .into_iter()
                                .collect();
                        if got != want {
                            failures.push(format!(
                                "[write] {w}\n  edges {:?}\n  got  {got:?}\n  want {want:?}",
                                g.edges
                            ));
                        }
                        *counts.entry("write").or_default() += 1;
                    }
                    Err(e) => failures.push(format!("[write] {w}: {e}")),
                }
            }
        }
        drop(db);
        drop(idb);
        let _ = std::fs::remove_dir_all(base.join(format!("g{gi}")));
        let _ = std::fs::remove_dir_all(base.join(format!("g{gi}i")));
    }
    let _ = std::fs::remove_dir_all(&base);
    let mut failed: BTreeMap<&str, usize> = BTreeMap::new();
    for f in &failures {
        let tag = f.split(']').next().unwrap_or("").trim_start_matches('[');
        *failed.entry(tag).or_default() += 1;
    }
    eprintln!("ql_ru randomized: cases {counts:?}, failures {failed:?}");
    assert!(
        failures.is_empty(),
        "{} of the cases differ; first ones:\n{}",
        failures.len(),
        failures
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    if graphs >= 120 {
        for shape in ["chain", "commas", "clauses", "optional", "write"] {
            assert!(
                counts.get(shape).copied().unwrap_or(0) > 100,
                "{shape}: {counts:?}"
            );
        }
    }
}
