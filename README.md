# graph-db

An embedded Rust property-graph database with native incremental linking
rules. You declare a predicate once; every later write maintains the
matching edges (and retracts them when properties change). The graph
builds itself.

Pre-alpha: single-writer, no node/edge deletes, no multi-statement
transactions. Toolchain is pinned to **1.92.0**
(`rust-toolchain.toml`). Apache-2.0.

## Quickstart

```text
cargo run -p core-api --example quickstart
```

The runnable copy is `crates/core-api/examples/quickstart.rs`. Same
story, condensed:

```rust
use core_api::{GraphDb, Predicate, RuleDef, Value};
use std::collections::BTreeMap;

fn tags(xs: &[&str]) -> Value {
    Value::List(xs.iter().map(|s| Value::Str((*s).into())).collect())
}

fn main() {
    let mut db = GraphDb::open(&std::env::temp_dir().join(format!("graphdb-quickstart-{}", std::process::id()))).expect("open");
    db.insert_node("Org", "acme", vec![("skills".into(), tags(&["graph", "rust", "search"]))]).expect("acme");
    db.insert_node("Org", "beta", vec![("skills".into(), tags(&["sales", "ops"]))]).expect("beta");
    db.create_rule(RuleDef {
        name: "skill_fit".into(), src_label: "Person".into(), dst_label: "Org".into(),
        predicate: Predicate::Overlap { field: "skills".into(), min: 0.5 },
        edge_type: "FIT".into(), weight_prop: Some("score".into()),
    }).expect("rule");
    db.insert_node("Person", "ada", vec![("skills".into(), tags(&["graph", "rust", "search"]))]).expect("ada");
    db.insert_node("Person", "bob", vec![("skills".into(), tags(&["graph", "rust"]))]).expect("bob");
    db.insert_node("Person", "cara", vec![("skills".into(), tags(&["sales"]))]).expect("cara");
    let mut params = BTreeMap::new();
    params.insert("min".into(), Value::Float(0.5));
    let _rs = db.query(
        "MATCH (p:Person)-[r:FIT]->(o:Org) WHERE r.score >= $min \
         RETURN p, o, r.score AS score ORDER BY score DESC, p",
        &params,
    ).expect("query");
    let _grouped = db.node_ref("ada").expect("ada").grouped_by_edge_type();
    let _why = db.explain("ada", "acme").expect("explain");
}
```

`Overlap` is Jaccard on a list-valued field. After the three `Person`
inserts the rule has already written three `FIT` edges (ada→acme 1.0,
bob→acme 2/3, cara→beta 0.5). Example output:

```text
== open ==
store: temp dir

== graph ==
nodes: 5  edges: 3  (derived FIT from skill_fit)

== query ==
columns: p, o, score
  p=ada  o=acme  score=1.0
  p=bob  o=acme  score=0.6666666666666666
  p=cara  o=beta  score=0.5

== grouped_by_edge_type (ada) ==
  FIT: acme

== explain (ada, acme) ==
  rule=skill_fit  type=FIT  ada→acme  weight=1.0
```

## What works today

| Area | Today |
|---|---|
| Storage | In-memory property graph; CRC-checksummed WAL (fsync per write); checksummed snapshots (`GDB1` / version 2); open = snapshot + WAL replay |
| Testing | Deterministic simulation (fault-injecting `SimFs`), crash-recovery, oracle equivalence, Cypher↔traversal equivalence |
| Linking rules | Declared `RuleDef`: `KeyMatch`, `FieldEqual`, `Overlap` (Jaccard), `All`. Incremental on `insert_node` / `set_prop`. Derived edges are not WAL-logged; replay re-fires the same `apply` path. Provenance via `explain`. `weight_prop` stores the score on the edge |
| Traversal | `node_ref`, `nodes_with_label`, `find_nodes` (`Filter`/`CmpOp`), `NodeRef::neighborhood`, `NodeRef::grouped_by_edge_type`, `neighbors` |
| Cypher | `GraphDb::query` — subset below |

**Cypher subset (v1):** one or more `MATCH` clauses; node pattern
`(var?:Label {k: literal or $param})`; relationships
`-[r?:TYPE]->`, `<-[r?:TYPE]-`, `-[r?:TYPE]-`; `WHERE` with `OR` /
`AND` / `NOT` and `= <> < <= > >=` on `var.field`, literals, `$params`;
`RETURN` var or `var.field` with optional `AS`; `ORDER BY` alias / var /
prop `ASC` or `DESC`; `SKIP n` / `LIMIT n`. Keywords case-insensitive;
identifiers `[A-Za-z_][A-Za-z0-9_]*`; strings in single quotes (`\'`
escape). Rel vars expose edge properties (`r.score`). Bare `RETURN v`
is the node key. Unknown label/type → zero rows.

Not in this subset: variable-length paths, aggregations, `OPTIONAL
MATCH`, writes via Cypher, functions, `DISTINCT`.

Multi-`MATCH` patterns whose variables are not joined (e.g. `MATCH (a)
MATCH (b) RETURN a, b`) produce a cross-join and will exhaust memory at
large node counts — constrain patterns with shared variables or add a
tight `LIMIT`.

## Coming later

- numeric / geo rule predicates
- node and edge deletes
- multi-statement transactions
- concurrent snapshot readers
- Arrow result sets
- server + UI
- language bindings

## Docs

- Design spec: [`docs/superpowers/specs/2026-08-14-graph-db-design.md`](docs/superpowers/specs/2026-08-14-graph-db-design.md)
- Plans: [`docs/superpowers/plans/`](docs/superpowers/plans/) — Plan 1 durable core, Plan 2 rule engine, Plan 3 query layer
