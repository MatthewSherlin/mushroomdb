# graph-db

An embedded Rust property-graph database with native incremental linking
rules. You declare a predicate once; every later write maintains the
matching edges (and retracts them when properties change). The graph
builds itself.

Pre-alpha: single-writer, no node/edge deletes, no multi-statement
transactions. Toolchain is pinned to **1.92.0**
(`rust-toolchain.toml`). Apache-2.0.

## Quickstart

The front door is the `graphdb` CLI (crate `cli`, binary name `graphdb`):

```text
cargo run -p cli --bin graphdb -- demo ./demo-db
cargo run -p cli --bin graphdb -- stats ./demo-db
cargo run -p cli --bin graphdb -- serve ./demo-db
cargo run -p cli --bin graphdb -- mcp ./demo-db
```

No-args and `--help` print usage. There is also a Rust-API walkthrough:

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
        max_edges: None,
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

## Demo

`graphdb demo <db-dir>` writes a **deterministic** generic dataset — 10
Orgs, 20 Projects, 30 People — via `ingest_json`. Rows carry list-valued
`skills` (a 3-token window `[s{home}, s{next}, s{next+1}]`) and FK fields
(`org_id`, `project_id`). Auto-FK `KeyMatch` rules are declared during
ingest; one scored `Overlap` rule (`skill_fit`, Jaccard ≥ 0.5 on
`skills`) is created after. Each person fully matches their home project
(score 1.0) and partially matches the two adjacent windows (score 0.5).
The command refuses a non-empty directory — including hidden files
(`.DS_Store` counts).

Captured from `cargo run -p cli --bin graphdb -- demo ./demo-db`:

```text
== demo ==
ingested 10 Orgs, 20 Projects, 30 People
overlap rule: skill_fit (Person.skills ∩ Project.skills, min 0.5)

== auto-FK rules ==
  auto_fk_person_org_id
  auto_fk_person_project_id
  auto_fk_project_org_id

== query ==
MATCH (p:Person {id: 'person-01'})-[r:FIT]->(proj:Project)
RETURN p, proj, r.score AS score
ORDER BY score DESC, proj

columns: p, proj, score
  p=person-01  proj=proj-01  score=1.0
  p=person-01  proj=proj-02  score=0.5
  p=person-01  proj=proj-20  score=0.5

== explain (person-01, proj-01) ==
  rule=auto_fk_person_project_id  type=PROJECT  person-01→proj-01  weight=none
  rule=skill_fit  type=FIT  person-01→proj-01  weight=1.0

== serve ==
  graphdb serve ./demo-db
```

`graphdb stats ./demo-db` after that demo:

```text
nodes: 60 live, 0 tombstoned
edges: 170
rules: 4
  auto_fk_person_org_id        edges=30  tripped=false
  auto_fk_person_project_id    edges=30  tripped=false
  auto_fk_project_org_id       edges=20  tripped=false
  skill_fit                    edges=90  tripped=false
```

A second `demo` into the same directory exits 1:

```text
demo refuses a non-empty directory: ./demo-db (directory must be empty — including hidden files)
```

## Server

`graphdb serve <db-dir> [--addr 127.0.0.1:0]` opens the store, binds
(default is ephemeral port 0), prints the bound address **after** the
listener is accepting, then serves. Real run against the demo dir:

```text
$ cargo run -p cli --bin graphdb -- serve ./demo-db
listening on http://127.0.0.1:59196
```

(Port `59196` is whatever the OS assigned for `:0`; pass
`--addr 127.0.0.1:8080` to pin one.)

Endpoints (thin wrappers over `core-api`):

| Method | Path | Notes |
|---|---|---|
| `POST` | `/query` | body `{"cypher","params?"}` → Arrow IPC (`application/vnd.apache.arrow.stream`); `?format=json` → `{"columns","rows"}` |
| `GET` | `/stats` | `Stats` JSON |
| `POST` | `/ingest` | body `{"label","rows","options?"}` → `IngestReport` JSON |
| `GET` | `/explain` | `?a=&b=` → `Explanation` array |
| `GET` | `/node/{key}/neighborhood` | `?depth=&dir=` |
| `GET` | `/watch` | WebSocket; one JSON text frame per post-commit `MutationEvent` |

`GET /stats` on the demo dataset (same process as the listen line above):

```text
{"edges":170,"nodes_live":60,"nodes_tombstoned":0,"rules":[...]}
```

## UI (in progress)

```text
cd ui && npm ci && npm run build
```

## MCP

`graphdb mcp <db-dir>` runs a newline-delimited JSON-RPC 2.0 loop on
stdio (no `Content-Length` framing). Methods: `initialize`,
`notifications/initialized`, `tools/list`, `tools/call`. Tools: `query`,
`ingest_json`, `explain`, `stats`, `neighborhood`.

```text
$ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    | cargo run -p cli --bin graphdb -- mcp ./demo-db
{"id":1,"jsonrpc":"2.0","result":{"capabilities":{"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"graph-db"}}}
```

## What works today

| Area | Today |
|---|---|
| Storage | In-memory property graph; CRC-checksummed WAL (fsync per write); checksummed snapshots (`GDB1` / version 2); open = snapshot + WAL replay |
| Testing | Deterministic simulation (fault-injecting `SimFs`), crash-recovery, oracle equivalence, Cypher↔traversal equivalence |
| Linking rules | Declared `RuleDef`: `KeyMatch`, `FieldEqual`, `Overlap` (Jaccard), `All`. Incremental on `insert_node` / `set_prop`. Derived edges are not WAL-logged; replay re-fires the same `apply` path. Provenance via `explain`. `weight_prop` stores the score on the edge |
| Traversal | `node_ref`, `nodes_with_label`, `find_nodes` (`Filter`/`CmpOp`), `NodeRef::neighborhood`, `NodeRef::grouped_by_edge_type`, `neighbors` |
| Cypher | `GraphDb::query` — subset below |
| Ingest | `ingest` / `ingest_json`; auto-FK `KeyMatch` on `*_id` |
| Concurrency | `SharedDb` — many readers or one writer (`RwLock`); lock-free epoch readers are Plan 8 |
| Arrow | `arrow-bridge`: `ResultSet` → RecordBatch / IPC stream |
| Server | HTTP + `/watch` WebSocket (`graphdb serve`) |
| MCP | stdio JSON-RPC (`graphdb mcp`) — agent-memory tools |
| CLI | `graphdb` — `serve` / `mcp` / `stats` / `demo` |

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

- numeric / geo / vector rule predicates
- node and edge deletes
- multi-statement transactions
- lock-free epoch snapshot readers (replacing the `RwLock` facade)
- UI (`graphdb ui`) + launch GIF
- language bindings

## Docs

- Design spec: [`docs/superpowers/specs/2026-08-14-graph-db-design.md`](docs/superpowers/specs/2026-08-14-graph-db-design.md)
- Plans: [`docs/superpowers/plans/`](docs/superpowers/plans/) — Plan 1 durable core, Plan 2 rule engine, Plan 3 query layer
