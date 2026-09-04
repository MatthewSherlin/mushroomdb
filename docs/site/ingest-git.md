# Codebase Graph (`ingest-git`)

`mushroomdb ingest-git <db-dir> <repo-dir>` turns a git repository into a graph
of authors, commits, and files, reads the working tree for the symbols each file
defines and the links between them, then derives every relationship from rules.
Re-running the same command against the same database syncs it: only the commits
after the recorded head are replayed, so deleted files drop out and renamed
files carry their history to the new path instead of leaving a stale node
behind.

```
mushroomdb ingest-git ~/.mushroomdb/code ~/src/myproject \
  --exclude 'target/' --exclude 'node_modules' --exclude '*.lock'
```

```
ingest-git: 620 commit(s), 394 file(s), 2 author(s)
  scanned 394 file(s): 5553 symbol(s), 429 import(s), 7484 call(s), 304 mention(s)
  hash-only 22  symbol cap hit on 0
  rules: auto_fk_commit_author_id, auto_fk_file_top_author_id, co_changed, knows,
         auto_fk_symbol_file_id, imports, calls, mentions, concept_sources, about_file
```

## Flags

| Flag | Effect |
|---|---|
| `--exclude <pattern>` | Skip matching paths. Repeatable; see [Exclusion patterns](#exclusion-patterns) |
| `--max-commits-per-file N` | Cap on the stored `commits` list per file (default 200) |
| `--recurse-submodules` | Also walk every initialised submodule, as its own unit |
| `--prs` | Ask `gh` for merged pull requests and link them to their commits |
| `--no-structure` | Skip the working-tree pass: no hashes, symbols, imports, calls or mentions |
| `--no-docs` | Skip Markdown bodies, headings and mentions; hashes and symbols still land |
| `--ensure-gitignore` | Add the database directory to the repository's `.gitignore` |

## Graph shape

| Label | key (`id`) | Props |
|---|---|---|
| `Author` | mailmap-resolved email | `name` |
| `Commit` | full sha | `message` (subject line), `ts` (unix seconds), `author_id`, `pr_id` (with `--prs`) |
| `File` | path | `path`, `dir`, `ext`, `commits` (list of shas, newest last, capped), `n_commits`, `top_author_id`, `author_counts`, and from the working tree: `hash`, `lines`, `lang`, `symbols_n`, `imports`, `import_lines`, `mentions`, `headings`, `body` |
| `Symbol` | `"<path>#<qualified name>"` | `name`, `kind`, `path`, `file_id`, `line_start`, `line_end`, `signature`, `doc`, `calls_to`, `call_lines` |
| `PR` | `"pr:<number>"` | `number`, `title`, `url`, `merged_at`, `author_login` |
| `GitSync` | `"__mushroomdb_git_sync__"` | `sha` — the last ingested commit, the marker the next run resumes from — plus `repo`, `recurse`, `prs`, `structure`, `docs` |

The sync marker's key is deliberately not `HEAD`. Node keys are one namespace
shared with `File` keys, which are repository paths, and a project that ships a
file named `HEAD` would otherwise collide with the marker.

Beside the resume sha the marker records how the ingest was run: `repo` is the
repository's absolute path, and `recurse`, `prs`, `structure` and `docs` are the
flags it was run under. A later run that changes one of them updates the marker
even when there is nothing new to walk.

## Author identity and `.mailmap`

Authors are keyed by the address git reports *after* applying the repository's
`.mailmap`, and `Author.name` is the mailmapped name. A contributor who has
committed from a work address and a personal one is therefore one `Author` node
with one set of `KNOWS` edges, as long as the repository maps the addresses
together:

```
Alice Example <alice@example.test> <alice.old@example.test>
```

Without a `.mailmap` nothing changes — the raw commit author name and address
are used, exactly as before.

Note that `n_commits` counts every commit that ever touched the file, while
`commits` holds only the most recent `--max-commits-per-file` shas (default
200). The cap bounds both node size and the cost of the overlap the
`co_changed` rule computes. Past the cap the two disagree on purpose:
`n_commits` keeps counting and the list stops growing, so a file with 900
commits reports `n_commits = 900` beside 200 shas. `author_counts` sums to
`n_commits`, not to the length of the list.

`author_counts` is the per-author commit distribution for that file, stored as
a list of `"email<TAB>count"` strings in email order. It is what makes
`top_author_id` correct across incremental runs: the walk only sees the new
window, so without the stored counts a second author's commits would restart
from zero on every sync and ownership could never change hands. Read it if you
want the full distribution; `top_author_id` is its argmax.

## Edges

| Edge | Direction | Source |
|---|---|---|
| `TOUCHED` | `Commit` → `File` | Written directly from `--name-status` output |
| `AUTHOR` | `Commit` → `Author` | Auto-FK on `Commit.author_id` |
| `TOP_AUTHOR` | `File` → `Author` | Auto-FK on `File.top_author_id` |
| `MERGED_AS` | `PR` → `Commit` | Written from the `gh` listing (`--prs`) |
| `PR` | `Commit` → `PR` | Auto-FK on `Commit.pr_id` (`--prs`) |
| `CO_CHANGED` | `File` → `File` | Rule `co_changed` |
| `KNOWS` | `Author` → `File` | Rule `knows` |
| `DEFINES` | `Symbol` → `File` | Foreign key on `Symbol.file_id` |
| `IMPORTS` | `File` → `File` | Rule `imports`, over the `imports` list |
| `CALLS` | `Symbol` → `Symbol` | Rule `calls`, over the `calls_to` list |
| `MENTIONS` | `File` → `File` | Rule `mentions`, over the `mentions` list |
| `DESCRIBED_IN` | `Concept` → `File` | Rule `concept_sources`, over the `source_files` list |
| `ABOUT` | `Note` → `File`/`Symbol`/`Author`/`Concept`/`Note` | Rule `about_<label>`, over the `about` list |

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
with files they own. `TOP_AUTHOR` is itself rule-derived, and rules chain, so
reassigning a file's `top_author_id` moves both edges in one write: the FK rule
rewrites `TOP_AUTHOR` and `knows` refires off it before the commit closes.

Both rules are declared once, on the first run, after the nodes exist — so each
backfills exactly once. The same holds for the structure rules below.
`File.path`, `Commit.message`, `Author.name`, `File.body`, `File.headings`,
`Symbol.name` and `Symbol.doc` are all indexed for full-text search.

## The working tree

Git says who changed what. It does not say what the code *is*. After the commit
walk, `ingest-git` reads every file the history left it — narrowed to the paths
that exist on disk right now — and records what it finds.

| On | Prop | Meaning |
|---|---|---|
| `File` | `hash` | First 16 bytes of the content BLAKE3 digest, 32 hex characters |
| `File` | `lines`, `lang` | Line count, and one of `rust`, `python`, `typescript`, `tsx`, `javascript`, `go`, `markdown`, `other` |
| `File` | `symbols_n` | How many `Symbol` nodes this file defines |
| `File` | `imports` | The `File` keys its imports resolve to |
| `File` | `import_lines` | `"<key><TAB><line>"` per import site — the evidence behind each edge |
| `Symbol` | `calls_to` | The `Symbol` keys its calls resolve to |
| `Symbol` | `call_lines` | `"<key><TAB><line>"` per call site |

Symbols are keyed `<path>#<qualified name>` — `src/store.rs#Store.put` — so two
files that both define `run` never collide, and a symbol carries its file in its
key. `file_id` names that file, and the foreign key on it is the `DEFINES` edge.

Resolution is deliberately conservative: an import that names something outside
the working tree (the standard library, a registry dependency) resolves to
nothing, and a call whose name has several candidate definitions and no clear
winner resolves to nothing. A missing edge is easier to live with than a wrong
one. Language rules are documented in the extraction crate.

Three things are read but not parsed. A file over 1 MB, a file whose leading
bytes are not text, and a file with an extension no extractor claims all keep
their `hash`, `lines` and `lang` and contribute nothing else. Past 2,000
definitions a single file stops contributing symbols; the run reports how many
files hit that.

**Only differences are written.** Each file's stored props are compared field by
field against what was just extracted, and its `Symbol` nodes against the
symbols just found in it. A file whose bytes have not changed produces no write
at all, so re-running over an unchanged tree leaves the database byte-identical.

**Renames rewrite both sides.** Renaming a file moves its node, but its old
`Symbol` keys can never be right again: they are deleted and re-created under
the new path. And a file that imported the old path has that path sitting in its
`imports` list, so every file naming a key that moved or vanished is extracted
again — which rewrites the list and lets the rule retract the edge.

`--no-structure` skips this pass entirely. Nothing is removed; the props simply
stop being maintained.

## Documentation

Markdown files contribute prose as well as structure. With `--docs` (the
default) a `.md` file stores its `body` (up to 64 KB), its `headings` in
document order, and its `mentions`: the other files it names, whether in a
backticked path or a relative link. A mention is a `MENTIONS` edge, so a
question about a file reaches the document that explains it.

`File.body` and `File.headings` are indexed for full-text search alongside
`File.path`, and `Symbol.name` and `Symbol.doc` alongside them, so a prompt that
uses a project's own vocabulary finds the file, the definition and the paragraph
that introduced it.

`--no-docs` skips all three: no body, no headings, no mentions. Hashes, symbols
and imports still land, since those are structure rather than prose.

## Notes and concepts

Two more rules are declared once their labels appear in the graph, so that
agent-written nodes join the same graph rather than sitting beside it:
`about_<label>` turns a `Note.about` list into `ABOUT` edges to files, symbols,
authors, concepts or other notes, and `concept_sources` turns a
`Concept.source_files` list into `DESCRIBED_IN` edges. Neither is created by
this command's own data — they appear on the first run after something writes a
`Note` or a `Concept`.

## Exclusion patterns

`--exclude` is repeatable and deliberately simple — no glob crate, three forms:

| Pattern | Means | Matches |
|---|---|---|
| ends with `/` | path prefix | `target/` matches `target/debug/x.rs`, not `targeted/x.rs` |
| starts with `*.` | file extension | `*.lock` matches `Cargo.lock`, not `Cargo.toml` |
| anything else | substring of the path | `node_modules` matches `ui/node_modules/x/y.js` |

**With no `--exclude` of your own, six defaults apply:** `target/`,
`node_modules/`, `dist/`, `.git/`, `*.lock` and `*.min.js`. These are the paths
a repository carries that are not its source — build output, vendored
dependencies, generated bundles, lockfiles nobody reads — and keeping them out
matters twice over now that the working tree is read: they would otherwise be
hashed and parsed on every run. Naming any pattern of your own replaces the
whole default set, so state the ones you still want.

Excluded paths are skipped entirely: no `File` node, no `TOUCHED` edge. A file
renamed *into* an excluded path is treated as deleted. Patterns are matched
against the key a file is stored under, which for a submodule includes the
submodule's path (`vendor/lib/src/lib.rs`), so `--exclude 'vendor/'` skips a
whole submodule.

## Submodules

Without `--recurse-submodules` a submodule contributes nothing. The gitlink the
parent records for it — the entry git reports as an ordinary change while it is
really a commit pointer — gets no `File` node either; the submodule paths listed
in `.gitmodules` are always skipped, initialised or not.

With `--recurse-submodules` every *initialised* submodule is walked as its own
unit. `git submodule foreach --recursive` decides which those are, so a
submodule that was never checked out is silently skipped rather than failing the
run. A unit has:

- **Keys under its path in the parent.** A submodule at `vendor/lib` stores its
  `src/lib.rs` as `vendor/lib/src/lib.rs`, so one query spans the whole tree and
  two submodules that both contain `src/lib.rs` never collide.
- **Its own sync marker**, keyed `__mushroomdb_git_sync__:<path>`. Each history
  advances independently: a commit in the submodule alone re-walks only the
  submodule.
- **Its own file state.** The parent's walk never sees a submodule's files, and
  a submodule's walk never sees the parent's.

Authors, commits and the rules are shared across units, so ownership and
co-change span the whole tree.

## Pull requests

`--prs` runs `gh pr list --state merged --limit 1000` in the repository and adds
a `PR` node per merged pull request. Two things link one to its commits:

- the **merge commit** the listing names, matched by sha;
- a **squash merge**, matched by the `(#123)` that GitHub appends to the
  subject line — only for numbers the listing actually returned.

A linked commit gets `pr_id`, from which the foreign-key rule derives the
`Commit` → `PR` edge, and the pull request gets a `MERGED_AS` edge to it.
Linking runs over every commit in the graph, not just the newly walked ones, so
adding `--prs` to a database that was ingested without it links its history on
the next run. `PR.title` is indexed for full-text search.

Everything about this step is best-effort: if `gh` is not installed, the
repository has no GitHub remote, or the user is not authenticated, the run
prints one line to stderr and continues. Pull requests are never a reason to
fail an otherwise complete ingest.

## Keeping the database out of the repository

`--ensure-gitignore` appends the database directory to the repository's
`.gitignore` — `mushroom-memory/` for `mushroomdb ingest-git ./mushroom-memory .`
— creating the file if it does not exist. It is idempotent: a second run finds
the line and changes nothing. A database stored outside the repository is left
alone, since the repository has no path to ignore.

## Incremental semantics

The first run has no `GitSync` node, so it reads the whole history and creates
the rules. Every later run reads the `sha` off the `GitSync` node and asks git
only for `<sha>..HEAD`, then applies the changes in order:

- **Added / modified** — the file's `commits` list, `n_commits`,
  `author_counts`, and `top_author_id` are updated, which re-derives its
  `CO_CHANGED` edges. Because the counts are cumulative and persisted,
  ownership moves the moment a second author passes the incumbent, and the
  answer matches a full re-ingest of the same repository.
- **Deleted** — the `File` node is deleted, so every edge touching it retracts.
  Its commits stay in the graph; only the file node goes.
- **Renamed** (git's `-M` detection) — the node is renamed, keeping its history,
  props, and edges, and its `id` prop follows the key. Chained renames inside
  one window collapse to a single move, and a file moved away and back again is
  no move at all.
- **Copied** — treated as a new file with no prior history.

The working-tree pass then runs over the paths that window touched, plus every
file whose `imports` or `mentions` list named a path that moved or vanished. A
first run, or a run whose recorded flags changed, scans the whole tree instead —
which is how turning `--no-docs` back off fills in the prose it skipped.

Only the path a file ends the window on decides its fate. A file renamed and
then deleted in the same window is deleted, not moved onto a dead path; a rename
onto a path another file just vacated replaces that file's node. Either way one
`File` node exists per live path, and its `id` always equals its key.

A run with no new commits writes nothing at all: `commit_seq` does not move.
That holds per unit — a run that only re-walks a submodule leaves the parent's
marker and file nodes untouched. Two things still count as work with no new
commits: `--prs`, which re-reads the pull request listing every time, and a
change to the recorded flags. Merge commits appear as `Commit` nodes with no
`TOUCHED` edges, since `--name-status` reports no changes for them by default.

Each run resolves `git rev-parse HEAD` before it walks anything, ends the walk at
that sha rather than at the symbolic `HEAD`, and records the same sha as the next
resume point. A commit landing while the run is in flight therefore falls outside
the range and is picked up by the following run, instead of being skipped by a
marker that had advanced past it. If the recorded head is no longer in the
repository (history was rewritten, or the database was pointed at a different
repo), the command fails rather than double-counting; ingest into a fresh
database directory. A repository with no commits yet reports zeros and writes
nothing.

Paths are read with `core.quotePath=false`, so non-ASCII filenames are stored as
written rather than octal-escaped. Git still quotes and escapes a path
containing a tab or a newline, and such a path is stored in that escaped form.
A commit subject containing a `0x1e` or `0x1f` byte truncates or drops that one
commit's `message`; the sha and the graph are unaffected.

## Concurrency

`ingest-git` takes the store's write lock for the duration of its write pass,
so it serialises against a running `mushroomdb serve`, a git hook, and any
other command touching the same directory. See
[`concurrency.md`](concurrency.md) for the model.

If another process holds the lock for longer than the wait budget, the command
prints

```
error: another mushroomdb process is writing; retry
```

and exits **3**, having written nothing. Retrying later is always safe: the run
either applies its whole window or none of it, and the sync marker only advances
with the data it covers.

## What the recall hook sees

Once `mushroomdb install` has wired the `UserPromptSubmit` recall hook at this
database, a prompt naming a file, a definition, an author, or words from a
commit message or a design document matches through the full-text indexes. The
hook then walks the graph outward, so a prompt about one file surfaces what it
imports, what calls into it, the files that change with it, the guide that
describes it and the person who owns it, before any file is read.

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
