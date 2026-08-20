# Contributing to mushroomdb

## Before you start

Read `README.md` and the design spec at
`docs/superpowers/specs/2026-08-14-graph-db-design.md` so you understand the
architecture, the generality guarantee, and the wire discipline.

---

## Gates — run these before every commit

### Rust gate (all commits touching `crates/` or `cli/`)

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --workspace --examples
cargo test --workspace
cargo bench --no-run
```

All five commands must exit 0. The bench step is compile-only (timing gates are
too noisy on CI runners).

### Node gate (commits touching `ui/`)

```text
cd ui
npm ci
npm run typecheck
npm test -- --run
npm run build
```

### Python gate (commits touching `bindings/python/`)

```text
cd bindings/python
python -m venv .venv
.venv/bin/pip install -U pip maturin pytest
.venv/bin/maturin develop
.venv/bin/pytest
```

### Embed gate (commits touching the `embed-ui` feature)

```text
cd ui && npm ci && npm run build && cd ..
cargo test -p server --features embed-ui
cargo build -p cli --bin mushroomdb --features embed-ui --release
```

---

## Testing philosophy — DST and oracle equivalence

### Deterministic simulation testing

The `sim-harness` crate replaces real disk IO with a fault-injecting
`SimFs` that runs under a virtual clock. Every crash scenario — torn WAL
write, fsync lie, corrupt snapshot tail — is a seeded, byte-for-byte
reproducible run. New storage or WAL code must be exercised under the
simulator: write failing sim tests first, then make them pass.

### Oracle equivalence

After any sequence of `insert_node` / `set_prop` / `create_rule` calls,
the set of derived edges produced by the incremental rule engine must
equal the set produced by a from-scratch `rebuild`. This invariant is
checked continuously in the property-test suite
(`crates/core-rules/tests/oracle.rs`). Any rule predicate addition must
extend the oracle path first.

### Differential Cypher testing

The Cypher executor is continuously tested against Neo4j on the supported
subset (see `benchmarks/test_harness.py` for the harness). New query
features must add a differential case. Known gaps (no LIMIT pushdown in
join materialization) are documented in `README.md` and may not be silently
introduced — surface new limitations explicitly.

---

## Append-only wire discipline

Existing HTTP and MCP endpoints have pinned golden shapes tested in
`crates/server/tests/`. The discipline is:

- **Never change** the shape, field names, or JSON type of an existing
  response field.
- **Never remove** a field from a response.
- **Add new optional fields** only (absent = old client unaffected).
- New endpoints may use any shape; they must add a golden shape test on
  the first commit that ships them.
- Error responses follow the register: `{"error": "<message>"}` with the
  appropriate HTTP status code (400 for bad input, 404 for not found, 500
  for engine errors).

MCP tool schemas follow the same rule: existing tool names and parameter
names are frozen; new optional parameters only.

---

## Code conventions

- Match the conventions already in the codebase; do not import new crates
  without raising it in an issue first.
- Types and functions are `snake_case`; structs and enums are `PascalCase`.
- Public API items require doc comments. Internal helpers do not need them,
  but complex logic should have an inline comment explaining the invariant.
- The generality guarantee from the design spec applies to all code: no
  engine behavior may depend on specific label, edge-type, or field names.
  Any label/type/field appearing in tests is illustrative.
- Derived edges are not WAL-logged. They are re-materialized on `open()`
  by replaying rule application over the node data. Do not add WAL entries
  for derived edges without a design discussion.

---

## Review expectations

- Open an issue before starting work on anything non-trivial so effort is
  not duplicated.
- Pull requests must pass all applicable gates (the CI jobs mirror the
  local gate list above).
- Benchmark changes require a result file update in `benchmarks/results/`
  with the machine details, version, and honesty caveats filled in.
- Security issues: see `SECURITY.md`. Do not open a public issue for a
  vulnerability.
