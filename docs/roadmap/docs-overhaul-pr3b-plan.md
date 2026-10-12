# Docs overhaul, pull request 3b — the HTTP page: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `docs/site/http.md` documents every route the server answers, a test fails when the page and the router disagree, and `api.md` stops being one 1,650-line page for three surfaces.

**Architecture:** `api.md` is taken apart by line range. Its HTTP half becomes `http.md`, regrouped, with twelve new route sections and the statements that were wrong against the handlers corrected. Its Python half becomes `python.md`, unchanged. Its Rust and CLI backup notes move to `durability.md`. What is left of `api.md` is a one-screen index of the reference pages. A Rust test reads the router's source and the page's headings and requires the two sets of routes to be equal.

**Tech Stack:** Markdown, one throwaway Python script for the move, a Rust integration test with no new dependency, the existing docs gates.

**Spec:** `docs/roadmap/docs-overhaul-spec.md`, "Pull request 3 — the reference".

## Global Constraints

- Text that moves, moves verbatim. The only edits to moved text are the corrections listed in Task 2, each checked against the handler or a live server on 2026-10-12.
- Every route section's heading is exactly `### METHOD /path`, with the path as the router spells it. No query string, no "(WebSocket)".
- `docs/site/*.md` stays flat; every new page is in `SUMMARY.md`, `index.md` and `SITE_DOCS`.
- Product behaviour does not change. Where the page and the code disagree, the page changes.
- No new dependency in any crate.
- Every existing gate stays green: `build-site.sh`, `test-build-site.sh`, `test-check-links.sh`, `check-links.py`, `check-claims.sh`, `gen-llms-full.sh --check`, `gen-reference.py --check`, `bump-version.sh --check`, `cargo test -p mushroomdb-server`.
- Commits use conventional prefixes, lower case, in the repository's sentence style, as the configured git user, with no co-author trailer.

**Two deviations from the spec.**

1. **Pull request 3's written half is itself split.** This plan is the HTTP page and the break-up of `api.md`. The TypeScript, Rust, Concepts and Vector search pages are 3c, with their own plan. They are new writing; this is mostly a move, and is reviewable as one.
2. **The route check reads the router's source, not the router.** The spec says "a test that reads the router's routes". axum's `Router` cannot be listed at run time, and exporting a route table from the server would be a product change. The test parses the `.route(…)` calls in `http.rs`.

## What the handlers say that the page does not

Read from `crates/server/src/http.rs`, `ws.rs` and `subscribe.rs`, and probed against `mushroomdb serve` on a `demo` store. These are the corrections Task 2 makes.

| Page says | Code does | Where |
|---|---|---|
| Role-bound tokens are "bearer-only" | Any token is read from the `Authorization: Bearer` header, then `?token=`, then the `mushroomdb_token` cookie | `http.rs` `request_token` |
| `POST /edges/upsert` is "Full-token only; role tokens return 403" | A role token is allowed within its write scope (`submit_batch_authz`) | `http.rs` `upsert_edge` |
| `POST /backup` "accepts any absolute path without validation" | `dest` is confined to `MUSHROOMDB_BACKUP_DIR`, else the server's working directory; `..` is refused | `http.rs` `confine_backup_dest` |
| `GET /explain` returns `src`, `dst`, `approximate` at the top level | It returns `src_key`, `dst_key`, `weight`, `via_edge` and a `predicate` object | probed |
| `GET /history/was_linked` takes a commit index | `at_commit` is a commit index or an RFC 3339 date | 0.7.1, ledger row 71 |
| `GET /stats` example | Also carries `chain_truncations` | probed |
| `GET /watch`: "pass `?token=`" | The header and the cookie work too; and the frame list omits nine event kinds | `ws.rs`, `MutationEvent` |
| The endpoint table lists 15 routes | The router declares 29: 27 and the two WebSocket routes | `http.rs` `build_app` |

## Review Focus

1. **A route is added to the router and not to the page.** Expected: `cargo test -p mushroomdb-server` fails and names the route. Pinned in Task 1.
2. **A route is removed or renamed and its section stays.** Expected: the same test fails and names the section. Pinned in Task 1.
3. **A link elsewhere in the repository to an `api.md` heading that moved.** Expected: `check-links.py` fails on it; none is left dangling. Pinned in Task 3.
4. **A refusal string that `doc_strings.rs` pins to `api.md`.** The string is now in `http.md` or `python.md`. Expected: every row points at the page that holds it, and the test is green. Pinned in Task 3.
5. **Text lost in the move.** Expected: every line of the old `api.md` is in exactly one of `http.md`, `python.md`, `durability.md`, or is on a short, stated list of lines deliberately dropped. Pinned in Task 2.

---

### Task 1: a test that holds the page to the router

**Files:**
- Create: `crates/server/tests/doc_routes.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: the test `the_http_page_and_the_router_name_the_same_routes`, which reads `docs/site/http.md`. Task 2 creates that file; until then the test fails, which is this task's red.

- [ ] **Step 1: Write the test**

Create `crates/server/tests/doc_routes.rs`:

```rust
//! The HTTP page and the router name the same routes.
//!
//! `docs/site/http.md` has one `### METHOD /path` section a route. Before this
//! test its endpoint table listed 15 routes of 29, and nothing said so. A route
//! added to the router with no section fails here by name; so does a section
//! left behind for a route that is gone.
//!
//! The routes are read from the router's source. axum's `Router` cannot be
//! listed at run time, and exporting a table from the server only so that a
//! test could read it would be a change to the product for the docs' sake.

use std::collections::BTreeSet;
use std::path::PathBuf;

const HTTP_RS: &str = include_str!("../src/http.rs");

/// The part of `http.rs` that builds the router: from the first
/// `Router::new()` to the end of the WebSocket router that follows it.
fn router_source() -> &'static str {
    let start = HTTP_RS
        .find("let app = Router::new()")
        .expect("http.rs no longer has `let app = Router::new()`");
    let streaming = HTTP_RS[start..]
        .find("let streaming = Router::new()")
        .expect("http.rs no longer has `let streaming = Router::new()`");
    let end = HTTP_RS[start + streaming..]
        .find(".with_state(")
        .expect("the streaming router no longer ends in `.with_state(`");
    &HTTP_RS[start..start + streaming + end]
}

/// `(METHOD, path)` for every `.route("path", method(handler))` in `src`.
fn routes_in(src: &str) -> BTreeSet<(String, String)> {
    const METHODS: [&str; 4] = ["get(", "post(", "put(", "delete("];
    let mut routes = BTreeSet::new();
    let mut rest = src;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route(".len()..];
        let open = rest.find('"').expect("a .route( with no path string");
        let len = rest[open + 1..].find('"').expect("an unterminated path string");
        let path = &rest[open + 1..open + 1 + len];
        let after = &rest[open + 1 + len..];
        let method = METHODS
            .iter()
            .filter_map(|m| after.find(m).map(|i| (i, *m)))
            .min()
            .map(|(_, m)| m.trim_end_matches('(').to_uppercase())
            .unwrap_or_else(|| panic!("no routing method after the path {path:?}"));
        routes.insert((method, path.to_string()));
    }
    routes
}

/// `(METHOD, path)` for every `### METHOD /path` heading in `page`.
fn routes_documented(page: &str) -> BTreeSet<(String, String)> {
    let mut routes = BTreeSet::new();
    for line in page.lines() {
        let Some(heading) = line.strip_prefix("### ") else { continue };
        let mut words = heading.split_whitespace();
        let (Some(method), Some(path)) = (words.next(), words.next()) else { continue };
        if !["GET", "POST", "PUT", "DELETE"].contains(&method) || !path.starts_with('/') {
            continue;
        }
        assert!(
            words.next().is_none(),
            "a route heading carries more than `### METHOD /path`: {line:?}"
        );
        routes.insert((method.to_string(), path.to_string()));
    }
    routes
}

fn http_page() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/site/http.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn the_reader_finds_a_route_split_across_lines() {
    let src = r#"
        .route("/health", get(health))
        .route(
            "/node/{key}/prop/{field}",
            axum::routing::put(set_node_prop),
        )
        .route("/node/{key}", axum::routing::delete(delete_node))
    "#;
    let found: Vec<_> = routes_in(src).into_iter().collect();
    assert_eq!(
        found,
        vec![
            ("DELETE".to_string(), "/node/{key}".to_string()),
            ("GET".to_string(), "/health".to_string()),
            ("PUT".to_string(), "/node/{key}/prop/{field}".to_string()),
        ]
    );
}

#[test]
fn a_heading_is_a_route_only_when_it_is_a_method_and_a_path() {
    let page = "### GET /health\n### Authentication\n#### GET /deeper\n### POST /nodes/{key}/rename\n";
    let found: Vec<_> = routes_documented(page).into_iter().collect();
    assert_eq!(
        found,
        vec![
            ("GET".to_string(), "/health".to_string()),
            ("POST".to_string(), "/nodes/{key}/rename".to_string()),
        ]
    );
}

#[test]
#[should_panic(expected = "more than `### METHOD /path`")]
fn a_route_heading_with_a_query_string_after_it_is_refused() {
    routes_documented("### GET /watch (WebSocket)\n");
}

#[test]
fn the_http_page_and_the_router_name_the_same_routes() {
    let routed = routes_in(router_source());
    assert!(
        routed.len() >= 29,
        "the reader found {} routes in http.rs; it found 29 when this test was written",
        routed.len()
    );
    let documented = routes_documented(&http_page());
    let show = |set: &BTreeSet<(String, String)>, other: &BTreeSet<(String, String)>| {
        set.difference(other)
            .map(|(method, path)| format!("{method} {path}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let missing = show(&routed, &documented);
    let stale = show(&documented, &routed);
    assert!(
        missing.is_empty() && stale.is_empty(),
        "docs/site/http.md and the router disagree.\n  routes with no `### METHOD /path` section: [{missing}]\n  sections for routes the server does not have: [{stale}]"
    );
}
```

- [ ] **Step 2: Run it to verify it fails for the right reason**

Run: `cargo test -p mushroomdb-server --test doc_routes 2>&1 | tail -15`
Expected: three tests pass — the two reader tests and the `should_panic` one — and `the_http_page_and_the_router_name_the_same_routes` fails with `cannot read …/docs/site/http.md: No such file or directory`. If instead the route count assertion fails, the reader is wrong: print `routes_in(router_source())` and fix it before going on.

- [ ] **Step 3: Commit**

```bash
git add crates/server/tests/doc_routes.rs
git commit -m "test(server): the http page and the router name the same routes"
```

The branch is red between this commit and the next. That is deliberate, and the pull request is not opened until Task 3 is done.

---

### Task 2: `api.md` is taken apart

**Files:**
- Create: `docs/site/http.md`, `docs/site/python.md`
- Modify: `docs/site/api.md` (becomes a short index), `docs/site/durability.md` (gains the Rust and CLI backup notes)

**Interfaces:**
- Consumes: `the_http_page_and_the_router_name_the_same_routes` from Task 1.
- Produces: `docs/site/http.md` with one `### METHOD /path` section for each of the 29 routes, and these `##` headings, which Task 3's link fixes point at: `Authentication`, `Write scopes for role-bound tokens`, `Limits, errors and request ids`. `docs/site/python.md`, whose `##` headings are `api.md`'s Python `###` headings promoted one level.

- [ ] **Step 1: Confirm the line ranges before cutting**

The move is by line number, so the numbers must be today's. Run:

```bash
grep -n '^## \|^### ' docs/site/api.md | awk -F: '{print $1": "$2}' | head -60
```

Expected, exactly: `10: ## HTTP API`, `23: ### Authentication`, `98: ### Write scopes for role-bound tokens`, `218: ### Endpoints`, `240: ### POST /query`, `373: ### GET /stats`, `412: ### GET /metrics`, `463: ### POST /ingest`, `503: ### POST /rules`, `573: ### GET /explain?a=&b=`, `604: ### GET /node/{key}`, `628: ### GET /node/{key}/edges`, `651: ### GET /node/{key}/neighborhood?depth=&dir=`, `666: ### POST /find_similar`, `698: ### POST /nodes/{key}/rename`, `725: ### POST /edges/upsert`, `756: ### POST /backup`, `802: ## Backup and export (Rust API and CLI)`, `882: ### GET /node/{key}/history`, `911: ### GET /history/edge?a=&b=`, `939: ### GET /history/was_linked?a=&b=&edge_type=&at_commit=`, `976: ### GET /watch (WebSocket)`, `993: ## MCP (Model Context Protocol)`, `1044: ## Python bindings`. If any number differs, stop: change the ranges in Step 2's `OLD` table to match before running it.

- [ ] **Step 2: Run the move**

This script is run once from the repository root and is not committed. `§§§` stands for a code fence, so that the script can sit inside one here.

```python
#!/usr/bin/env python3
"""One-off: take docs/site/api.md apart into http.md, python.md and durability.md."""
import re
from pathlib import Path

SITE = Path("docs/site")
api = (SITE / "api.md").read_text(encoding="utf-8").split("\n")
used: set[int] = set()


def old(first: int, last: int, heading: str | None = None) -> str:
    """Lines first..last of api.md (1-based, inclusive), their trailing rule
    dropped, and — when `heading` is given — their first line replaced by it."""
    lines = api[first - 1 : last]
    used.update(range(first, last + 1))
    while lines and lines[-1].strip() in ("", "---"):
        lines.pop()
    if heading is not None:
        lines[0] = heading
    return "\n".join(lines)


NEW = {}

NEW["limits"] = """## Limits, errors and request ids

- **Body size.** A request body may be at most 64 MiB.
- **Deadline.** A request that has not finished in 120 seconds is answered `408 Request Timeout`
  and dropped. The two WebSocket routes are exempt: a subscription is meant to outlive any
  request.
- **Concurrency.** At most 256 requests are in flight at once. The next one is answered
  `503 Service Unavailable` straight away; nothing queues. The WebSocket routes do not count.
- **Request id.** Every response carries `x-request-id`. Send your own, up to 128 ASCII
  characters, and it is echoed; otherwise the server makes one.

An error is one JSON object, `{"error": "<what went wrong>"}`, whatever the status:

| Status | When |
|---|---|
| 400 | The request is malformed, or the engine refused it: an unknown key, a duplicate key, a rule-owned edge, a query that does not parse |
| 401 | A token is configured and the request carries none, or one the server does not know. Body: `{"error":"unauthorized"}` |
| 403 | A role-bound token asked for something outside its role. The message says what |
| 404 | `POST /nodes/{key}/rename` on a key that does not exist; a node read by a role that may not see it; a path that is not a route, on a server with no explorer to fall back to |
| 409 | `POST /nodes/{key}/rename` onto a key that is taken |
| 415 | A route that takes a JSON body was sent without `Content-Type: application/json` |
| 500 | The store is corrupt, or `roles.json` is misconfigured |
| 503 | Another process holds the write lock (`Retry-After: 1`), or the server is at its concurrency limit |

Two things are not that object: the 415 and 408 bodies, which are plain text from the HTTP
layer, and a failed `POST /backup`, which answers 500 with its report.

Most writes to a missing key are 400, not 404: `DELETE /node/{key}`, the property routes and
`POST /edges` all answer `400 {"error":"node key not found: <key>"}`. Only rename maps it to 404."""

NEW["health"] = """### GET /health

Liveness. It needs no token and never answers 401, so a load balancer can call it.

§§§json
{"ok": true}
§§§

With the full-access token — or on a server started with no token at all — it also says what
it is serving:

§§§json
{"ok": true, "nodes": 60, "edges": 334, "addr": "127.0.0.1:8080"}
§§§

A role-bound token, a wrong token and no token all get `{"ok": true}` and nothing more: the
counts say how big the graph is, and `/stats` refuses a role the same counts."""

NEW["suggest"] = """### GET /suggest

Rules the store proposes from its own values. It takes no parameters, reads the whole graph
under a five-second budget, and creates nothing: pass a suggestion's `def` to `POST /rules` to
create it. The same store always gets the same answer.

§§§json
{
  "suggestions": [
    {
      "def": {
        "name": "suggest_fe_person_person_org_id",
        "src_label": "Person",
        "dst_label": "Person",
        "predicate": {"FieldEqual": {"field": "org_id"}},
        "edge_type": "SAME_ORG_ID",
        "max_edges": 32,
        "weight_prop": null,
        "approximate": false
      },
      "est_edges": 60,
      "examples": [["person-04", "person-14", 1.0]],
      "rationale": "…"
    }
  ],
  "truncated": false
}
§§§

`truncated` is true when the budget ran out before every candidate was scored. A role-bound
token gets 403: the scan would reveal nodes outside its role. More in
[Rule suggestion](suggest.md)."""

NEW["post_nodes"] = """### POST /nodes

Create one node.

§§§json
{"label": "Person", "key": "ada", "props": {"name": "Ada", "skills": ["rust"]}, "namespace": "team-a"}
§§§

`label` and `key` are required. `props` is an object; a `null` value in it is dropped.
`namespace` is optional and is the node's namespace for life.

§§§json
{"ok": true, "nodes": 1, "edges": 0}
§§§

`nodes` and `edges` count what the write created, rule-derived edges included.

It is an insert, not an upsert: a key that exists is `400 {"error":"duplicate node key: ada"}`.
A missing field is `400 {"error":"missing label"}` or `"missing key"`. A role-bound token may
create a node whose label is in its `create_labels`; otherwise 403 with the reason."""

NEW["delete_node"] = """### DELETE /node/{key}

Delete a node and the edges on it. `200 {"ok": true}`.

A key that does not exist is `400 {"error":"node key not found: <key>"}`. A role-bound token
may delete a node it can see whose label is in its `delete_labels`; otherwise 403.

History keeps the node: `GET /node/{key}/history` still answers for it until the log is
pruned. See [Time travel](timetravel.md)."""

NEW["put_prop"] = """### PUT /node/{key}/prop/{field}

Set one property. Every rule that reads the field is re-evaluated in the same write.

§§§json
{"value": ["rust", "graphs"]}
§§§

`value` is required and may be any JSON value but `null`; to remove a property, use `DELETE`.
`200 {"ok": true}`.

400 when the key does not exist, when `value` is missing or `null`, or when the field is one a
materialized view owns. A role-bound token may set a property on a node it can see whose label
is in its `update_labels`; otherwise 403."""

NEW["delete_prop"] = """### DELETE /node/{key}/prop/{field}

Remove one property. It takes no body. `200 {"ok": true}` — also when the node has no such
field, which is not an error.

400 when the key does not exist or a materialized view owns the field. A rule that read the
field retracts the edges it no longer supports, in the same write. Role-bound tokens: as for
`PUT`."""

NEW["post_edges"] = """### POST /edges

Create one edge between two nodes that exist.

§§§json
{"type": "KNOWS", "src": "ada", "dst": "grace"}
§§§

All three are required. `200 {"ok": true}` — also when the edge is already there.

A missing endpoint is `400 {"error":"node key not found: grace"}`; to have it created, use
`POST /edges/upsert`, whose body names the same three things differently (`edge_type`,
`src_key`, `dst_key`). A role-bound token may create an edge whose type is in its
`create_edge_types` between two nodes it can see; otherwise 403."""

NEW["delete_edge"] = """### DELETE /edges/{etype}/{src}/{dst}

Delete one edge that was written by hand. `200 {"ok": true}` — also when there is no such edge
between the two nodes.

An edge a rule derived cannot be deleted this way:

§§§json
{"error": "edge is rule-owned: edge FIT person-01→proj-01 is rule-owned; delete or change the owning rule"}
§§§

That is 400, as is an endpoint that does not exist. A role-bound token may delete an edge whose
type is in its `delete_edge_types` between two nodes it can see; otherwise 403."""

NEW["algo"] = """### POST /algo/pagerank

PageRank over hand-written and derived edges together. The body is a JSON object and may be
`{}`: every field has a default.

| Field | Default | What it is |
|---|---|---|
| `damping` | 0.85 | The damping factor |
| `max_iters` | 50 | Iterations before giving up |
| `tol` | 1e-6 | Stop when no score moves by more |
| `direction` | `"out"` | `"out"`, `"in"` or `"both"` |
| `edge_type` | all | Count only edges of this type |
| `weight_prop` | none | Weight each edge by this property |
| `min_weight` | none | Ignore edges whose weight is below it |
| `budget_ms` | 5000 | Stop after this long; 0 for no limit |

§§§json
{"scores": [["org-07", 0.1029], ["org-03", 0.0882]], "converged": true}
§§§

Scores are highest first. `converged` is false when the iterations or the budget ran out first.

A value the config does not accept is 400, and the message names what it would have taken:

§§§json
{"error": "invalid pagerank config: unknown variant `sideways`, expected one of `out`, `in`, `both`"}
§§§

A role-bound token gets 403 on every `/algo` route, because the scan covers the whole graph.
More in [Algorithms](algorithms.md).

### POST /algo/wcc

Weakly connected components. Fields: `edge_type`, `weight_prop`, `min_weight` and `budget_ms`,
as for PageRank; `{}` is valid.

§§§json
{"components": [["org-01", "org-01"], ["org-02", "org-01"]], "truncated": false}
§§§

Each pair is a node and its component, named by the smallest key in it. `truncated` is true
when the budget ran out.

### POST /algo/degree

Degree centrality. Fields: `direction` (default `"both"`), `edge_type`, `weight_prop`,
`min_weight` and `budget_ms`; `{}` is valid.

§§§json
{"scores": [["org-04", 11], ["org-05", 11]], "truncated": false}
§§§

Highest first."""

NEW["subscribe"] = """### GET /subscribe

A WebSocket that streams what rules do, and optionally every write and the rows of one query.
After the upgrade, send one text frame saying what you want:

§§§json
{"rules": ["skill_fit"], "writes": true, "cypher": "MATCH (p:Person) RETURN p.name LIMIT 50"}
§§§

Every field is optional. `rules` names the rules whose edges to follow, `writes` adds node,
edge and property events, and `cypher` follows the rows of one query, re-run on each commit —
give it a `LIMIT`. The server answers `{"subscribed": true}`, or `{"error": "…"}` and closes
when a rule is unknown or the query does not parse.

Then one frame an event, each with a `type` and the `commit_seq` it happened at:

| `type` | Fields |
|---|---|
| `edge_fired` | `rule`, `src_key`, `dst_key`, `edge_type`, `weight` |
| `edge_retracted` | `rule`, `src_key`, `dst_key`, `edge_type` |
| `node_inserted` | `label`, `key` |
| `node_deleted` | `key` |
| `edge_inserted`, `edge_deleted` | `edge_type`, `src`, `dst` |
| `prop_set`, `prop_removed` | `key`, `field` |
| `query_row_added`, `query_row_removed` | `columns`, `row` |

A consumer that falls behind is not disconnected. It gets `{"type": "lagged", "missed": 12}`
for the events it did not see, and the stream goes on.

Only writes made through this server process are seen: a commit another process made is
readable at once and announces nothing. A role-bound token gets 403, because the stream has
no mask. More in [Subscriptions](subscriptions.md)."""

ROUTES = [
    ("Service", [
        ("GET", "/health", "Liveness; with the full-access token, node and edge counts"),
        ("GET", "/stats", "Node, edge and rule counts"),
        ("GET", "/metrics", "Process and store metrics, as JSON"),
        ("POST", "/backup", "A verified copy of the store, taken while it serves"),
    ]),
    ("Query, ingest and rules", [
        ("POST", "/query", "Run a Cypher read or write"),
        ("POST", "/ingest", "Insert many nodes of one label"),
        ("POST", "/rules", "Declare a rule"),
        ("GET", "/suggest", "Rules the store proposes from its own values"),
        ("GET", "/explain", "Why two nodes are linked: the rules, scores and values"),
        ("POST", "/find_similar", "Nearest nodes to a vector"),
    ]),
    ("Reading a node", [
        ("GET", "/node/{key}", "A node's label and properties"),
        ("GET", "/node/{key}/edges", "The edges on a node, with the rule behind each"),
        ("GET", "/node/{key}/neighborhood", "What is within a few hops"),
        ("GET", "/node/{key}/history", "Every change to a node"),
    ]),
    ("History between two nodes", [
        ("GET", "/history/edge", "When edges between two nodes came and went"),
        ("GET", "/history/was_linked", "Whether two nodes were linked on a date or at a commit"),
    ]),
    ("Writing nodes and edges", [
        ("POST", "/nodes", "Create a node"),
        ("DELETE", "/node/{key}", "Delete a node"),
        ("PUT", "/node/{key}/prop/{field}", "Set a property"),
        ("DELETE", "/node/{key}/prop/{field}", "Remove a property"),
        ("POST", "/nodes/{key}/rename", "Change a node's key"),
        ("POST", "/edges", "Create an edge between two nodes that exist"),
        ("POST", "/edges/upsert", "Create an edge, and its endpoints if they are missing"),
        ("DELETE", "/edges/{etype}/{src}/{dst}", "Delete a hand-written edge"),
    ]),
    ("Graph algorithms", [
        ("POST", "/algo/pagerank", "PageRank"),
        ("POST", "/algo/wcc", "Weakly connected components"),
        ("POST", "/algo/degree", "Degree centrality"),
    ]),
    ("Live streams", [
        ("GET", "/watch", "WebSocket: every committed mutation"),
        ("GET", "/subscribe", "WebSocket: rule firings, and optionally writes and a query's rows"),
    ]),
]

SECTION = {
    ("GET", "/health"): NEW["health"],
    ("GET", "/stats"): old(373, 411),
    ("GET", "/metrics"): old(412, 462),
    ("POST", "/backup"): old(756, 801),
    ("POST", "/query"): old(240, 372),
    ("POST", "/ingest"): old(463, 502),
    ("POST", "/rules"): old(503, 572),
    ("GET", "/suggest"): NEW["suggest"],
    ("GET", "/explain"): old(573, 603, "### GET /explain"),
    ("POST", "/find_similar"): old(666, 697),
    ("GET", "/node/{key}"): old(604, 627),
    ("GET", "/node/{key}/edges"): old(628, 650),
    ("GET", "/node/{key}/neighborhood"): old(651, 665, "### GET /node/{key}/neighborhood"),
    ("GET", "/node/{key}/history"): old(882, 910),
    ("GET", "/history/edge"): old(911, 938, "### GET /history/edge"),
    ("GET", "/history/was_linked"): old(939, 975, "### GET /history/was_linked"),
    ("POST", "/nodes"): NEW["post_nodes"],
    ("DELETE", "/node/{key}"): NEW["delete_node"],
    ("PUT", "/node/{key}/prop/{field}"): NEW["put_prop"],
    ("DELETE", "/node/{key}/prop/{field}"): NEW["delete_prop"],
    ("POST", "/nodes/{key}/rename"): old(698, 724),
    ("POST", "/edges"): NEW["post_edges"],
    ("POST", "/edges/upsert"): old(725, 755),
    ("DELETE", "/edges/{etype}/{src}/{dst}"): NEW["delete_edge"],
    ("POST", "/algo/pagerank"): NEW["algo"],
    ("GET", "/watch"): old(976, 992, "### GET /watch"),
    ("GET", "/subscribe"): NEW["subscribe"],
}
ALGO_IN_ONE = {("POST", "/algo/wcc"), ("POST", "/algo/degree")}  # written under pagerank's entry


def anchor(method: str, path: str) -> str:
    slug = re.sub(r"[^\w\- ]", "", f"{method} {path}".lower()).replace(" ", "-")
    return f"#{slug}"


start = old(12, 22)  # the lines under "## HTTP API"
auth = old(23, 97, "## Authentication")
scopes = old(98, 217, "## Write scopes for role-bound tokens")
old(218, 239)  # the 15-route table: replaced by the table of all 29 below
old(1, 11)     # the old title and introduction: rewritten for api.md below
old(993, 1043)  # the MCP section: a pointer to mcp.md, which api.md's new index is

table = ["| Method | Path | What it does |", "|---|---|---|"]
body = []
for group, routes in ROUTES:
    body.append(f"## {group}")
    for method, path, what in routes:
        table.append(f"| `{method}` | [`{path}`]({anchor(method, path)}) | {what} |")
        if (method, path) not in ALGO_IN_ONE:
            body.append(SECTION[(method, path)])

http = "\n\n".join([
    "# HTTP API",
    "Every route `mushroomdb serve` answers. Requests and responses are JSON unless a section says\n"
    "otherwise; `POST /query` answers Arrow by default.",
    "## Starting the server",
    start,
    auth,
    scopes,
    NEW["limits"],
    "## Routes",
    "\n".join(table),
] + body) + "\n"
(SITE / "http.md").write_text(http.replace("§§§", "```"), encoding="utf-8")

python = old(1044, len(api))
python = re.sub(r"(?m)^###(#*) ", r"##\1 ", python)
python = python.replace("## Python bindings", "# Python guide", 1)
(SITE / "python.md").write_text(python + "\n", encoding="utf-8")

durability = (SITE / "durability.md").read_text(encoding="utf-8").rstrip("\n")
moved = old(802, 881, "## Backup and export from Rust and the CLI")
(SITE / "durability.md").write_text(durability + "\n\n" + moved + "\n", encoding="utf-8")

unused = [n for n in range(1, len(api) + 1) if n not in used and api[n - 1].strip() not in ("", "---")]
assert not unused, f"lines of api.md that went nowhere: {unused[:20]}"

(SITE / "api.md").write_text("""# API reference

mushroomdb is one engine behind several surfaces. Each has its own page.

| Surface | Reference | Guide |
|---|---|---|
| HTTP, served by `mushroomdb serve` | [HTTP API](http.md) — every route | [Running as a service](service.md) |
| MCP, served by `mushroomdb mcp` | [MCP tools](mcp-tools.md) — every tool and argument | [Agent memory](mcp.md) |
| Python, `pip install mushroomdb` | [Python API](python-api.md) — every method | [Python guide](python.md) |
| The `mushroomdb` binary | [CLI](cli.md) — every subcommand | [Quickstart](quickstart.md) |
| TypeScript, `npm install mushroomdb-client` | [the client's README](https://github.com/MatthewSherlin/mushroomdb/blob/main/clients/typescript/README.md) | |

This page used to hold the HTTP and Python references together. Its sections are now in
[HTTP API](http.md), [Python guide](python.md) and, for backup and export from Rust and the
CLI, [Durability](durability.md#backup-and-export-from-rust-and-the-cli).
""", encoding="utf-8")

print("http.md:", len(http.split("\n")), "lines;", sum(len(r) for _, r in ROUTES), "routes")
```

Save it as a file outside the repository (your scratch directory) and run it with `python3`. Expected: `http.md: <about 1,300> lines; 29 routes`. If the `assert` fires it lists line numbers of `api.md` that went to none of the three pages: read those lines, and either extend a range or, if they are a rule or a heading that was replaced, add an `old(a, b)` call beside the other discards.

- [ ] **Step 3: Make the corrections**

Each is an exact replacement in `docs/site/http.md`. If a search string is not found exactly once, stop and read the section: the text differs from what this plan was written against.

(a) Role tokens are not bearer-only. Replace

```text
bearer-only. A role token receives a node-visibility mask derived from the
```

with

```text
sent the same three ways. A role token receives a node-visibility mask derived from the
```

and replace

```text
**Full-access token** (`--token` / `MUSHROOMDB_TOKEN`): bearer or `?token=`
query param. Grants access to all endpoints.
```

with

```text
**Full-access token** (`--token` / `MUSHROOMDB_TOKEN`): sent as an `Authorization: Bearer`
header, as a `?token=` query parameter, or in the `mushroomdb_token` cookie, which the server
sets when a browser opens the explorer with `?token=`. The header is read first, then the
query, then the cookie. Grants access to all endpoints.
```

(b) `POST /edges/upsert` allows role tokens. Replace

```text
Insert an edge, auto-creating any missing endpoint nodes. Full-token only; role tokens
return 403.
```

with

```text
Insert an edge, auto-creating any missing endpoint nodes. A role-bound token may do so when
the edge type is in its `create_edge_types` and it can see both endpoints; otherwise 403.
```

(c) `POST /backup` confines its destination. Replace the paragraph

```text
`dest` is an arbitrary admin-supplied path; the server must have write permission to
it. The endpoint accepts any absolute path without validation — restrict access to
this endpoint accordingly.
```

with

```text
`dest` must fall inside the backup root: `MUSHROOMDB_BACKUP_DIR` if it is set, otherwise the
directory the server was started in. A relative `dest` is resolved under the root; an absolute
one must already be inside it; a `..` segment is refused. Anything else is 400, and the
message names the root. The server must be able to write there.
```

(d) `GET /explain` returns what it returns. Replace the whole JSON block under `### GET /explain` — from the line ` [` after `Response: array of explanation objects:` to its closing `]` — with:

```json
[
  {
    "rule": "skill_fit",
    "edge_type": "FIT",
    "src_key": "person-01",
    "dst_key": "proj-01",
    "weight": 1.0,
    "via_edge": null,
    "predicate": {
      "kind": "overlap",
      "fields": ["skills"],
      "min": 0.5,
      "tolerance": null,
      "km": null,
      "parts": null,
      "approximate": false
    }
  }
]
```

and add after that block:

```text
One object for each rule-derived edge between the two keys. `predicate.kind` is `key_match`,
`field_equal`, `overlap`, `numeric_within`, `geo_radius`, `vector_similar`, `all` or `any`; the
fields that do not apply to a kind are `null`. Both keys are required, and a key that does not
exist is `400 {"error":"node key not found: <key>"}`.
```

Check the kind names against the code before keeping the sentence: `grep -n '"key_match"\|"field_equal"\|"numeric_within"\|"geo_radius"\|"vector_similar"' crates/core-api/src/*.rs crates/core-rules/src/*.rs | head`. Use the spellings the code has, and drop any the code does not.

(e) `GET /history/was_linked` takes a date. Replace

```text
Point-in-time check: was an edge of `edge_type` between `a` and `b` active at WAL
commit `at_commit` (0-based)?
```

with

```text
Point-in-time check: was an edge of `edge_type` between `a` and `b` active at `at_commit`?
`at_commit` is a 0-based WAL commit index, or an RFC 3339 date — `2026-06-19`, or
`2026-06-19T12:00:00Z` — which resolves to the last commit at or before it. All four
parameters are required. The `at_commit` in the response is the commit index it resolved to.
```

(f) `GET /stats` carries `chain_truncations`. In the JSON example under `### GET /stats`, replace the line `  "history_floor": 0,` with the two lines `  "history_floor": 0,` and `  "chain_truncations": 0,`.

(g) `GET /watch`: the token, and the frames. Replace

```text
When the server is started with a token, pass `?token=` on the WebSocket URL.
```

with

```text
The other frames are `prop_removed`, `edge_inserted` and `edge_deleted` (`edge_type`, `src`,
`dst`), `node_deleted` (`key`), `rule_created`, `rule_deleted` and `rule_rebuilt` (`name`),
`batch_applied` (`ops`) and `ingested` (`label`, `inserted`). The lag notice here is
`{"lagged": n}`; on `/subscribe` it is `{"type": "lagged", "missed": n}`.

When the server is started with a token, send it as a header or a cookie, or as `?token=` on
the WebSocket URL when the client cannot set headers. A role-bound token gets 403.
```

- [ ] **Step 4: Run Task 1's test**

Run: `cargo test -p mushroomdb-server --test doc_routes 2>&1 | tail -8`
Expected: `test result: ok. 4 passed`. A failure lists routes with no section or sections with no route: fix the heading in `docs/site/http.md`, not the test.

- [ ] **Step 5: Prove the test catches both directions**

```bash
sed -i.bak 's|^### GET /suggest$|### GET /sugest|' docs/site/http.md
cargo test -p mushroomdb-server --test doc_routes 2>&1 | grep -A3 "disagree"
mv docs/site/http.md.bak docs/site/http.md
```

Expected: `routes with no ### METHOD /path section: [GET /suggest]` and `sections for routes the server does not have: [GET /sugest]`.

- [ ] **Step 6: Check nothing was lost**

Every non-blank line of the old `api.md` should now be in one of the three pages, apart from the lines the plan replaces. Run:

```bash
git show HEAD:docs/site/api.md > /tmp/api-old.md
cat docs/site/http.md docs/site/python.md docs/site/durability.md > /tmp/api-new.md
python3 - <<'EOF'
old = [l.strip() for l in open("/tmp/api-old.md", encoding="utf-8") if l.strip() and l.strip() != "---"]
new = set(l.strip() for l in open("/tmp/api-new.md", encoding="utf-8"))
promote = lambda l: l[1:] if l.startswith("###") else l
gone = [l for l in old if l not in new and promote(l) not in new]
print(len(gone), "lines of the old page are in none of the new ones:")
for l in gone:
    print("  ", l[:150])
EOF
```

Expected: a list of about 70 lines, all of them one of: the old title and four-line introduction; `## HTTP API`, `### Endpoints` and the fifteen rows of the old endpoint table; the section headings that lost a query string or a suffix; the `## MCP` section's lines; and the lines Step 3 replaced on purpose. Read the list. Any line that is none of those is text that was lost: find where it should go and put it there.

- [ ] **Step 7: Commit**

```bash
git add docs/site/http.md docs/site/python.md docs/site/api.md docs/site/durability.md
git commit -m "docs(http): every route has a section, and what the page said wrongly about tokens and backups is corrected"
```

---

### Task 3: everything that pointed at `api.md` points at the right page

**Files:**
- Modify: `docs/site/SUMMARY.md`, `docs/site/index.md`, `scripts/gen-llms-full.sh`, `crates/server/tests/doc_strings.rs`, `docs/site/masks.md`, `docs/site/service.md`, `docs/site/algorithms.md`, `scripts/reference/python-api.md.tmpl`, `scripts/reference/cli.md.tmpl`, `llms.txt`, `CONTRIBUTING.md`, `README.md`, `docs/landing/index.html`
- Regenerate: `docs/site/python-api.md`, `docs/site/cli.md`, `llms-full.txt`

**Interfaces:**
- Consumes: `docs/site/http.md` and `docs/site/python.md` and their headings from Task 2.
- Produces: nothing later tasks use.

- [ ] **Step 1: The sidebar, the index, `llms-full.txt`**

In `docs/site/SUMMARY.md`, replace the line `- [HTTP and Python API](api.md)` with:

```markdown
- [Overview](api.md)
- [HTTP API](http.md)
```

and, in the `# Guides` group, after the line `- [Agent memory](mcp.md)`, add:

```markdown
- [Python guide](python.md)
```

In `docs/site/index.md`, replace the line beginning `- [API reference](api.md)` with:

```markdown
- [API reference](api.md) — which page covers which surface
- [HTTP API](http.md) — every route `mushroomdb serve` answers
- [Python guide](python.md) — the binding, with worked examples
```

In `scripts/gen-llms-full.sh`, in `SITE_DOCS`, after the line `    api.md`, add the two lines `    http.md` and `    python.md`.

- [ ] **Step 2: Let the link gate find every anchor that moved**

Run: `python3 scripts/check-links.py`
Expected: it fails, with `ANCHOR` lines for the links that named a heading of the old `api.md`. As of this plan they are:

| File | Link | Becomes |
|---|---|---|
| `docs/site/masks.md:90` | `api.md#vector-search` | `python.md#vector-search` |
| `docs/site/masks.md:105` | `api.md#write-scopes-for-role-bound-tokens` | `http.md#write-scopes-for-role-bound-tokens` |
| `docs/site/service.md:102` | `api.md#post-backup` | `http.md#post-backup` |
| `docs/site/algorithms.md:115` | `api.md#degree` | `python.md#degree` |
| `scripts/reference/python-api.md.tmpl:20` | `api.md#python-bindings` | `python.md` |

Fix each. For any other `ANCHOR` line it prints, find the heading in `http.md` or `python.md` (`grep -n '^#' docs/site/http.md docs/site/python.md`) and point the link there. Links to bare `api.md` still resolve; change the ones whose sentence is about one surface: in `scripts/reference/cli.md.tmpl`, `[API reference](api.md)` becomes `[HTTP API](http.md)`; in `scripts/reference/python-api.md.tmpl`, the link text `[API guide]` becomes `[Python guide]`.

Then regenerate the two generated pages and check:

```bash
cargo build -p mushroomdb-cli --bin mushroomdb
python3 scripts/gen-reference.py --binary target/debug/mushroomdb cli python-api
python3 scripts/check-links.py
```

Expected: `check-links.py: OK`.

- [ ] **Step 3: Move `doc_strings.rs`'s rows to the page that holds each string**

`crates/server/tests/doc_strings.rs` pins twelve refusal strings to `api.md`. Each is now in `http.md` or `python.md`. Run this to see where, from the repository root:

```bash
python3 - <<'EOF'
import re
from pathlib import Path
src = Path("crates/server/tests/doc_strings.rs").read_text(encoding="utf-8")
pages = {p: Path(f"docs/site/{p}").read_text(encoding="utf-8") for p in ("http.md", "python.md", "api.md")}
for needle in re.findall(r'"api\.md",\s*\n\s*"((?:[^"\\]|\\.)*)"', src):
    text = bytes(needle, "utf-8").decode("unicode_escape")
    print([p for p, body in pages.items() if text in body], repr(text[:70]))
EOF
```

Expected: every needle is found in exactly one of `http.md` and `python.md`, and none in `api.md`. In `doc_strings.rs`, change each row's `"api.md"` to the page printed for it, change the section comment `// docs/site/api.md` to name both pages, and update the module comment's `docs/site/{api,masks,rules,mcp}.md` to `docs/site/{http,python,masks,rules,mcp}.md`. A needle found in neither page is a sentence the move dropped: Task 2, Step 6 should have caught it — restore the sentence, do not delete the row.

Run: `cargo test -p mushroomdb-server --test doc_strings --test doc_routes 2>&1 | grep "test result"`
Expected: two lines, both `ok`.

- [ ] **Step 4: The entry points**

In `README.md`, in the line beginning `- **Reference:**`, replace `[HTTP and Python guide](https://matthewsherlin.github.io/mushroomdb/docs/api.html)` with `[HTTP API](https://matthewsherlin.github.io/mushroomdb/docs/http.html)`; and in the line beginning `- **Guides:**`, add ` · [Python](https://matthewsherlin.github.io/mushroomdb/docs/python.html)` after the Agent memory link. In the paragraph that ends `or over HTTP ([API reference](…api.html)).`, leave the link: `api.html` is now the page that says which surface is where.

In `docs/landing/index.html`, the top bar's `Reference` link stays `docs/api.html` for the same reason.

In `llms.txt` (lines 263 and 507) and `CONTRIBUTING.md` (line 74), read each sentence that names `api.md`: where it means the HTTP endpoints, change the link to `docs/site/http.md`.

- [ ] **Step 5: Regenerate `llms-full.txt`, build, run every gate**

```bash
bash scripts/gen-llms-full.sh && bash scripts/gen-llms-full.sh --check
bash scripts/build-site.sh
bash scripts/tests/test-build-site.sh
bash scripts/tests/test-check-links.sh
python3 scripts/check-links.py
bash scripts/check-claims.sh
bash scripts/bump-version.sh --check
python3 -m unittest scripts/tests/test_gen_reference.py
python3 scripts/gen-reference.py --check --binary target/debug/mushroomdb
cargo test -p mushroomdb-server
```

Expected: all green; `build-site.sh: OK — 36 pages`. An mdBook warning naming `http.md` or `python.md` is HTML-like text that sat harmlessly in `api.md` only if it was already there — it was built before, so a new warning comes from a new section: put the phrase in backticks.

- [ ] **Step 6: Look at the page**

```bash
python3 -m http.server 8765 --directory _site >/dev/null 2>&1 &
```

Open `http://localhost:8765/docs/http.html`. Check: the routes table's 29 links each jump to their section; the sidebar shows the seven groups; the `/algo/pagerank` field table and the `/subscribe` event table read cleanly; on a phone width the routes table scrolls inside the page. Open `docs/api.html`: the five-row table links to five pages that exist. Stop the server: `kill %1`.

- [ ] **Step 7: Commit**

```bash
git add docs/site scripts/gen-llms-full.sh scripts/reference llms-full.txt llms.txt \
        crates/server/tests/doc_strings.rs README.md CONTRIBUTING.md docs/landing/index.html
git commit -m "docs(reference): what pointed at the old api page points at the page that holds it"
```
