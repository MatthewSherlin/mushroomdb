# Codebase Graph (`ingest-git`)

`mushroomdb ingest-git <db-dir> <repo-dir>` turns a git repository into a graph
of authors, commits, and files, then derives co-change and ownership edges from
rules. Re-running the same command against the same database syncs it: only the
commits after the recorded head are replayed, so deleted files drop out and
renamed files carry their history to the new path instead of leaving a stale
node behind.

```
mushroomdb ingest-git ~/.mushroomdb/code ~/src/myproject \
  --exclude 'target/' --exclude 'node_modules' --exclude '*.lock'
```

```
ingest-git: 1204 commit(s), 318 file(s), 7 author(s)
  rules: auto_fk_commit_author_id, auto_fk_file_top_author_id, co_changed, knows
```

## Graph shape

| Label | key (`id`) | Props |
|---|---|---|
| `Author` | email | `name` |
| `Commit` | full sha | `message` (subject line), `ts` (unix seconds), `author_id` |
| `File` | path | `path`, `dir`, `ext`, `commits` (list of shas, newest last, capped), `n_commits`, `top_author_id`, `alive` |
| `GitSync` | `"__mushroomdb_git_sync__"` | `sha` — the last ingested commit, the marker the next run resumes from |

The sync marker's key is deliberately not `HEAD`. Node keys are one namespace
shared with `File` keys, which are repository paths, and a project that ships a
file named `HEAD` would otherwise collide with the marker.

Note that `n_commits` counts every commit that ever touched the file, while
`commits` holds only the most recent `--max-commits-per-file` shas (default
200). The cap bounds both node size and the cost of the overlap the
`co_changed` rule computes.

## Edges

| Edge | Direction | Source |
|---|---|---|
| `TOUCHED` | `Commit` → `File` | Written directly from `--name-status` output |
| `AUTHOR` | `Commit` → `Author` | Auto-FK on `Commit.author_id` |
| `TOP_AUTHOR` | `File` → `Author` | Auto-FK on `File.top_author_id` |
| `CO_CHANGED` | `File` → `File` | Rule `co_changed` |
| `KNOWS` | `Author` → `File` | Rule `knows` |

`top_author_id` is whoever has the most commits on that file. Ties break on the
lexicographically smallest email so repeated runs agree.

## The two rules

**`co_changed`** — `Overlap { field: "commits", min: 0.25 }`, edge `CO_CHANGED`,
weight `score`, at most 10 edges per file. Two files link when the jaccard
overlap of their commit lists is at least 0.25, meaning they are usually
changed together. The weight is that overlap, so `ORDER BY r.score DESC`
ranks the tightest couplings first.

**`knows`** — the same predicate as a via-hop: `via_label: "File"`,
`via_edge: "TOP_AUTHOR"`, `via_dir: In`, edge `KNOWS`, at most 20 edges per
author. From an author, the hop expands incoming `TOP_AUTHOR` edges to the
files they own, evaluates the overlap between each owned file and every other
file, and links the author to what co-changes with their code. So an author
`KNOWS` files they may never have committed to, as long as those files move
with files they own.

Both rules are declared once, on the first run, after the nodes exist — so each
backfills exactly once. `File.path`, `Commit.message`, and `Author.name` are
also indexed for full-text search on that first run.

## Exclusion patterns

`--exclude` is repeatable and deliberately simple — no glob crate, three forms:

| Pattern | Means | Matches |
|---|---|---|
| ends with `/` | path prefix | `target/` matches `target/debug/x.rs`, not `targeted/x.rs` |
| starts with `*.` | file extension | `*.lock` matches `Cargo.lock`, not `Cargo.toml` |
| anything else | substring of the path | `node_modules` matches `ui/node_modules/x/y.js` |

Excluded paths are skipped entirely: no `File` node, no `TOUCHED` edge. A file
renamed *into* an excluded path is treated as deleted.

## Incremental semantics

The first run has no `GitSync` node, so it reads the whole history and creates
the rules. Every later run reads the `sha` off the `GitSync` node and asks git
only for `<sha>..HEAD`, then applies the changes in order:

- **Added / modified** — the file's `commits` list, `n_commits`, and
  `top_author_id` are updated, which re-derives its `CO_CHANGED` edges.
- **Deleted** — the `File` node is deleted, so every edge touching it retracts.
  Its commits stay in the graph; only the file node goes.
- **Renamed** (git's `-M` detection) — the node is renamed, keeping its history,
  props, and edges, and its `id` prop follows the key. Chained renames inside
  one window collapse to a single move, and a file moved away and back again is
  no move at all.
- **Copied** — treated as a new file with no prior history.

Only the path a file ends the window on decides its fate. A file renamed and
then deleted in the same window is deleted, not moved onto a dead path; a rename
onto a path another file just vacated replaces that file's node. Either way one
`File` node exists per live path, and its `id` always equals its key.

A run with no new commits writes nothing at all: `commit_seq` does not move.
Merge commits appear as `Commit` nodes with no `TOUCHED` edges, since
`--name-status` reports no changes for them by default.

Each run records `git rev-parse HEAD` as the next resume point, so the range the
following run asks for is exact. If the recorded head is no longer in the
repository (history was rewritten, or the database was pointed at a different
repo), the command fails rather than double-counting; ingest into a fresh
database directory. A repository with no commits yet reports zeros and writes
nothing.

Paths are read with `core.quotePath=false`, so non-ASCII filenames are stored as
written rather than octal-escaped. Git still quotes and escapes a path
containing a tab or a newline, and such a path is stored in that escaped form.
A commit subject containing a `0x1e` or `0x1f` byte truncates or drops that one
commit's `message`; the sha and the graph are unaffected.

## What the recall hook sees

Once `mushroomdb install` has wired the `UserPromptSubmit` recall hook at this
database, a prompt naming a file, an author, or words from a commit message
matches through the full-text indexes on `File.path`, `Author.name`, and
`Commit.message`. The hook then walks the graph outward, so a prompt about one
file surfaces the files that change with it and the person who owns it, before
any file is read.

The same data answers direct questions:

```
mushroomdb query <db-dir> "MATCH (a:File)-[r:CO_CHANGED]->(b:File)
  RETURN a.id, b.id, r.score ORDER BY r.score DESC LIMIT 5"

mushroomdb query <db-dir> "MATCH (f:File {id: 'src/lib.rs'})-[:TOP_AUTHOR]->(a:Author)
  RETURN a.name, a.id"
```

The `explain` endpoint (`GET /explain?a=<src>&b=<dst>`, also an MCP tool) names
the rule and the score behind any derived edge, so a `CO_CHANGED` link is always
traceable back to the shared commits.
