# Contributing to mushroomdb

## Before you start

Read `README.md` and the design spec at `docs/design.md` so you understand the
architecture, the generality guarantee, and the wire discipline.

---

## Toolchain

The Rust toolchain is pinned in `rust-toolchain.toml` (currently 1.92.0). `rustup` picks
it up automatically when you run `cargo` from the repository root. Install rustup from
<https://rustup.rs/> if you do not already have it; the correct toolchain version is
downloaded on first use.

Node (18+) is required for the UI gate. Python 3.9+ and `maturin` are required for the
Python bindings gate.

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

### TypeScript client gate (commits touching `clients/typescript/`)

```text
cd clients/typescript && npm ci && npm run typecheck && npm test
```

### Embed gate (commits touching the `embed-ui` feature)

```text
cd ui && npm ci && npm run build && cd ..
cargo test -p mushroomdb-server --features embed-ui
cargo build -p mushroomdb-cli --bin mushroomdb --features embed-ui --release
```

### How to run the suite

`cargo test --workspace` is the whole Rust suite, including the deterministic
simulation tests and the oracle equivalence property tests. It takes a few
minutes on a laptop. To iterate on one area, scope it: `cargo test -p
mushroomdb-cli`, `cargo test -p mushroomdb-server`, or a single test by name
(`cargo test --workspace edge_history`). Run the full gate before you push —
CI mirrors it exactly.

---

## Distribution and packaging

The user-facing install paths live in `README.md`. The paths below are for
contributors building or publishing the artifacts themselves.

Tags `v0.4.1`, `v0.4.2`, `v0.4.3`, `v0.4.4`, `v0.4.5`, `v0.5.0`, and `v0.5.1` are published; `npx mushroomdb`,
the `curl install.sh`, and `ghcr.io/matthewsherlin/mushroomdb` are all live today.

### Build the embedded-UI binary from source

```text
cd ui && npm ci && npm run build && cd ..
cargo build -p mushroomdb-cli --bin mushroomdb --features embed-ui --release
cp target/release/mushroomdb ~/.local/bin/  # or any directory on PATH
```

Or run directly from the source tree (no copy needed):

```text
./target/release/mushroomdb demo ./db && ./target/release/mushroomdb serve ./db
```

Without the embedded binary (cargo only, debug build):

```text
cargo run -p mushroomdb-cli --bin mushroomdb -- demo ./demo-db
cargo run -p mushroomdb-cli --bin mushroomdb -- serve ./demo-db
```

### Docker

```text
docker run --rm -p 8080:8080 -e MUSHROOMDB_TOKEN=… ghcr.io/matthewsherlin/mushroomdb
```

The image CMD runs `mushroomdb serve /data --addr 0.0.0.0:8080 --demo-if-empty`
(writes the demo graph into the volume when empty, then serves). Non-loopback
bind requires a token; pass `-e MUSHROOMDB_TOKEN=…` and open
`http://localhost:8080/?token=…`.
Explicit two-step:

```text
docker run --rm -v mushroomdb-data:/data ghcr.io/matthewsherlin/mushroomdb demo /data
docker run --rm -p 8080:8080 -e MUSHROOMDB_TOKEN=… -v mushroomdb-data:/data ghcr.io/matthewsherlin/mushroomdb serve /data --addr 0.0.0.0:8080
```

Local image build:

```text
docker build -t mushroomdb:local .
docker run --rm -p 8080:8080 -e MUSHROOMDB_TOKEN=… mushroomdb:local
```

### curl / install.sh

```text
curl -fsSL https://raw.githubusercontent.com/MatthewSherlin/mushroomdb/main/packaging/install.sh | sh
```

Writes `~/.local/bin/mushroomdb` (no sudo). Fetches the matching GitHub
Release tarball and checksum-verifies it.

### npm

```text
npx mushroomdb --help
```

### TypeScript client (install from repo)

The `mushroomdb-client` package wraps the HTTP + WebSocket API with full TypeScript types.
It is not yet published to npm. Install from the repository:

```sh
npm install /path/to/graph-db/clients/typescript
# or in package.json:
# "mushroomdb-client": "file:../path/to/graph-db/clients/typescript"
```

```ts
import { MushroomClient } from 'mushroomdb-client';

const client = new MushroomClient('http://127.0.0.1:8080');
const result = await client.query('MATCH (p:Person) RETURN p.id LIMIT 5');
console.log(result.rows);
```

See [`clients/typescript/README.md`](clients/typescript/README.md) for the full quickstart, API reference, and WebSocket subscription docs.

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
(`crates/sim-harness/tests/oracle_equivalence.rs`). Any rule predicate
addition must extend the oracle path first.

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
