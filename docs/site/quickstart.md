# Quickstart

Two commands start a populated graph explorer.

---

## Requirements

- Rust toolchain 1.92.0 (pinned in `rust-toolchain.toml`; `rustup` will
  install it automatically on first `cargo` run in the repo)
- The repo cloned locally

---

## Source build (available now)

Build the release binary with the UI embedded:

```text
cd ui && npm ci && npm run build && cd ..
cargo build -p mushroomdb-cli --bin mushroomdb --features embed-ui --release
```

Run the two-command flow:

```text
./target/release/mushroomdb demo ./db
./target/release/mushroomdb serve ./db
```

Open `http://127.0.0.1:8080/` in a browser. The explorer loads the demo
graph: 10 Orgs, 20 Projects, 30 People, 334 edges (including 7 derived
rule sets).

You can combine both commands on one line:

```text
./target/release/mushroomdb demo ./db && ./target/release/mushroomdb serve ./db
```

Expected output:

```text
== demo ==
ingested 10 Orgs, 20 Projects, 30 People
overlap rule: skill_fit (Person.skills ∩ Project.skills, min 0.5)
numeric rule: founded_within (Org.founded_year, tolerance 2)
geo rule: nearby_office (Org.office [lat,lon], 50 km)
vector rule: similar_interests (Person.embedding dim 8, min 0.8)

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
  rule=auto_fk_person_project_id  type=PROJECT  person-01→proj-01  weight=1.0
  rule=skill_fit  type=FIT  person-01→proj-01  weight=1.0

== serve ==
  mushroomdb serve ./db
listening on http://127.0.0.1:8080
```

---

## Using the explorer

- The empty-state "Load demo neighborhood" button fetches one node
  (`MATCH (n) RETURN n LIMIT 1` — resolves to `org-01`). Query
  `person-01` explicitly to see the scored FIT neighborhood.
- Click any edge to open the why panel, which shows which rule fired,
  the predicate values, and the computed score.
- The Rules tab lists every active rule with its edge count.
- The Console tab accepts Cypher queries.

---

## Without the embedded UI

A debug binary (no `--features embed-ui`) is API-only unless you pass
`--ui ui/dist`:

```text
cargo run -p mushroomdb-cli --bin mushroomdb -- demo ./demo-db
cargo run -p mushroomdb-cli --bin mushroomdb -- serve ./demo-db --ui ui/dist
```

Or API-only (no browser):

```text
cargo run -p mushroomdb-cli --bin mushroomdb -- serve ./demo-db
```

---

## Wire it into an assistant

From the repository you want graphed:

```text
mushroomdb install
```

With no flags it detects the assistant (Claude Code, Cursor, or both), picks
project scope inside a git checkout and user scope anywhere else, and prints
which it chose. Project scope writes the MCP entry to `.mcp.json`, the
`/mushroom` skill to `.claude/skills/mushroom/`, the prompt hooks to
`.claude/settings.json`, an ignore line for the store, and a backgrounded
`sync` into the `post-commit`, `post-checkout` and `post-merge` git hooks.

The MCP entry runs `npx -y mushroomdb@<version>`, so the assistant needs
nothing installed globally. To point it at a local build instead:

```text
mushroomdb install --project --platform claude-code \
  --command ./target/release/mushroomdb --no-prewarm
```

Other flags: `--user`, `--platform codex` (registers through the Codex CLI),
`--db <path>`, and `--no-git-hooks`. `mushroomdb uninstall` removes exactly
what was written. Full reference: [`skill.md`](skill.md).

Restart the assistant afterwards — MCP servers and hooks are read at startup.

---

## Rust API

For a programmatic walkthrough from Rust:

```text
cargo run -p mushroomdb --example quickstart
```

Source: `crates/core-api/examples/quickstart.rs`.

---

## Distribution (after the first v* tag)

After the first tagged release, these one-liners will be available:

```text
# Docker (non-loopback requires a token)
docker run --rm -p 8080:8080 -e MUSHROOMDB_TOKEN=changeme ghcr.io/matthewsherlin/mushroomdb
# then open http://localhost:8080/?token=changeme

# npm
npx mushroomdb --help

# curl
curl -fsSL https://raw.githubusercontent.com/MatthewSherlin/mushroomdb/main/packaging/install.sh | sh
```

These are not available until the tag is pushed. See the Distribution
section in `README.md` for details.

---

## Check the stats

```text
mushroomdb stats ./db
```

Output after the demo:

```text
nodes: 60 live, 0 tombstoned
edges: 334
rules: 7
  auto_fk_person_org_id        edges=30  tripped=false
  auto_fk_person_project_id    edges=30  tripped=false
  auto_fk_project_org_id       edges=20  tripped=false
  founded_within               edges=34  tripped=false
  nearby_office                edges=16  tripped=false
  similar_interests            edges=114  tripped=false
  skill_fit                    edges=90  tripped=false
```

---

## Deployment and TLS

To serve over HTTPS — via a reverse proxy (nginx, Caddy) or the built-in
`--features tls` rustls path — see [deployment.md](deployment.md).

---

## Known first-run issues

- `demo` refuses a non-empty directory, including hidden files (`.DS_Store`
  counts). Use a fresh path or `rm -rf ./db` first.
- Default bind is `127.0.0.1:8080`. Pass `--addr host:port` to change it.
  Non-loopback binds require `--token` or `MUSHROOMDB_TOKEN`.
- Cold-start on a rich-rule graph: WAL-only open replays all rule derivations (8.16 min at 100k
  nodes, 9 rules, IVF dominates). Call `snapshot()` before close; opening from a V6 snapshot takes
  8.88 s at 100k (snapshot write cost: 22.563 s). See [docs/site/timetravel.md](timetravel.md).
