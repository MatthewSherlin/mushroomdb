# Docs overhaul — spec

Written 2026-10-12, after v0.7.1 merged at `31da934`. Status: awaiting the owner's review.
Nothing in this spec has been built.

## Why

A visitor cannot read the documentation on the website. `pages.yml` copies `docs/site/*` as it
is, so the site is one hand-written landing page (`index.html`) whose only links go to GitHub,
and the 24 Markdown pages beside it are served as plain text (`quickstart.md` answers
`content-type: text/markdown`). The README carries what the site does not show: about 6,200
words, with its Install section starting at line 165. The reference material that exists is
incomplete against the code — the HTTP endpoint table lists 15 of the router's 29 routes, the
Python "exposed surface" about 20 of roughly 70 methods — and the CLI reference lives only in
the README.

## Goal

The project reads as a finished one to someone who arrives cold:

- they understand what it is from one sentence and can run a first command within a screen;
- every documentation page is a rendered page, reachable from the landing page, with
  navigation and search;
- each public surface has one reference page that matches the code, and cannot drift from it
  without CI failing;
- the pages share one voice.

## Decisions taken

| Decision | Choice | Decided by |
|---|---|---|
| Site generator | mdBook, run only in the Pages workflow and a CI build check | owner, 2026-10-12 |
| The one pitch | "The graph that stays true — and knows who's allowed to see it." | owner, 2026-10-12 |
| Order | site, README, reference, tone — four pull requests | owner, 2026-10-12 |
| Source of truth | `docs/site/*.md` stays the source, flat, one directory | this spec |
| Old URLs | the raw `.md` files stay published at their current paths | this spec |
| Visual direction | the landing page's existing palette, applied to the docs | this spec |

The last three are this spec's, and are open to change at review.

**Flat source.** `gen-llms-full.sh`, `check-links.py`, `check-claims.sh` and
`crates/server/tests/doc_strings.rs` all read `docs/site/*.md` by a flat glob. Grouping is done
in the sidebar, not in directories, so those gates keep working with list edits only.

**Old URLs.** `llms.txt` and outside links may point at `…/mushroomdb/<page>.md`. Publishing
the raw files beside the rendered ones costs one `cp` and keeps every such link alive; an agent
fetching a page still gets Markdown.

**Visual direction.** Warm off-white `#fbfaf8`, rust accent `#8a3324`, system sans-serif,
automatic dark mode — what `index.html` has now. The social card uses a different palette (dark
navy, amber, teal, monospace); it is redrawn to match in the fourth pull request.

## Out of scope

- Any change to product behaviour. Where a page and the code disagree, the page changes.
- A versioned docs site (one set of pages per release). The site documents `main`.
- Hosted rustdoc. The Rust page is a hand-written overview that links to docs.rs.
- An OpenAPI document. The HTTP page is checked against the router by a test, not generated
  from a schema. An OpenAPI spec is a product feature and belongs on the roadmap.
- The TypeScript client's missing routes (it covers 12 of 29). The page documents what exists.

## The site's shape

URLs: the landing page stays at the site root; the book is published under `/docs/`.

Sidebar (`docs/site/SUMMARY.md`), with new pages marked:

| Group | Pages |
|---|---|
| Start | Introduction (`index.md`), Quickstart, Install (**new**, from `skill.md` and the README), Concepts (**new**) |
| Guides | Agent memory, Rules, Query, Time travel, Access control, Full-text search, Vector search (**new**, gathered from `rules.md`, `indexes.md`, `api.md`), Indexes, Views, Subscriptions, Algorithms, Rule suggestion, Git ingest |
| Reference | CLI (**new**), MCP tools (**new**), HTTP API (**new**, from `api.md`), Python (**new**, from `api.md`), TypeScript (**new**), Rust (**new**) |
| Operate | Running as a service, Deployment, Durability, Concurrency (absorbs `multiprocess.md`), Known limits (**new**, from the README), Benchmarks (absorbs the README's section and `association-bench.md`) |
| Project | Architecture (**new**, from the README), Testing, Panic policy, Format stability, Changelog (link) |

**Concepts** is the page the owner asked for — "how things work". One page, about 1,200 words,
one short section each: a store is a directory; nodes, keys and labels; edges; rules derive and
retract edges; why two things are related (explain); time (commits, as-of reads); who can see
what (namespaces, roles, scopes); memory (`remember`, `recall`, `forget`, provisional
entities); search (text and vector). Each section ends with a link to its guide. It defines
every term the other pages use before they use it.

## Pull request 1 — the site renders

Content does not change in this pull request beyond links.

- `book.toml` at the repository root: `src = "docs/site"`, title, the repository URL, an
  edit-this-page link, search on, the custom stylesheet.
- `docs/site/SUMMARY.md` with the groups above, listing only the pages that exist today.
- The landing page moves out of the book's source to `docs/landing/index.html`, because mdBook
  renders `index.md` to `index.html` and copies other files across, and the two would collide.
  It gains a top bar (Docs, Quickstart, Reference, GitHub), the pitch, an install block with a
  copy button, and Open Graph tags pointing at the social card.
- `docs/site/theme/` (or the path `book.toml` names): one stylesheet carrying the palette, type
  and code-block style.
- Links that leave the book (`../../README.md`, `../format-stability.md`, `../assets/…`) are
  rewritten so they resolve on the rendered site: assets are staged under the book, and
  repository files are linked by absolute GitHub URL. (`format-stability.md` moves into the
  site in pull request 4, with the pages that cite it; until then it is linked the same way.)
- `pages.yml`: install a pinned mdBook release, `mdbook build` into `_site/docs/`, copy the
  landing page and `docs/assets` into `_site/`, copy the raw `docs/site/*.md` into `_site/`.
  Its `paths` filter gains `book.toml` and `docs/landing/**`.
- `ci.yml`: a job that runs `mdbook build` with the same pinned version, so a page that does
  not build fails the pull request rather than the deploy.
- Gate edits, each one line or one list entry: `check-claims.sh` reads the landing page at its
  new path; `gen-llms-full.sh` excludes `SUMMARY.md`; `check-links.py` takes `SUMMARY.md` as a
  second index, so a page missing from the sidebar fails as a page missing from `index.md`
  does.

Done when: the deployed landing page links into the docs; every page in `docs/site` is in the
sidebar and renders; search finds "rename_node"; the old `.md` URLs still answer; all gates
are green.

## Pull request 2 — the README

Target: about 800 words, in this order.

1. Name, the pitch, three badges (CI, version, licence).
2. Two sentences: what it is, who it is for.
3. Install — three lines, one per route (`npx`, `pip`, `cargo`).
4. A first command with its output shown (`demo`, then one `query`).
5. What makes it different — the rule example and its retraction, about 120 words.
6. What it is not — four lines.
7. Where to go next — links into the rendered site, one line each.
8. Status and licence.

Everything else moves, not disappears: Agent memory to the agent-memory guide; the CLI table to
the CLI reference (as a stopgap until pull request 3 generates it); Known limitations and
Benchmarks to their Operate pages; Architecture to its Project page; role tokens to access
control; the Python exception list to `bindings/python/README.md`.

Also in this pull request, because they are entry points too:

- `llms.txt` is brought into line (it says PyPI and npm are unpublished).
- `packaging/npm/README.md` is added — the npm page for `mushroomdb` is blank today.
- `crates/cli/README.md` stops contradicting the root README about the explorer UI.
- Inbound anchors are kept or their links updated: `README.md#install` (`index.md:64`,
  `quickstart.md:367`), `README.md#architecture` (`CONTRIBUTING.md:5`, `docs/design.md:13`,
  `index.md:48`).
- `llms-full.txt` is regenerated.

The root README is the crates.io page for `mushroomdb`. Its links are absolute, so they work
there. The change appears on crates.io at the next publish, not before.

Done when: the README is under 900 words, the first command is on the first screen, no
sentence in it names a past version, and all gates are green.

## Pull request 3 — the reference

One page a surface. Three are generated, three written; all are checked.

| Page | Source | Check |
|---|---|---|
| `cli.md` | generated from `mushroomdb --help` (`usage()`, `crates/cli/src/lib.rs:422`) plus a hand-written line of context per group | generator `--check` in CI |
| `mcp-tools.md` | generated from the live `tools/list` reply, both listings, by extending `scripts/measure-tool-listing.py`: name, the one-line question, every argument with type and whether required | generator `--check` in CI |
| `python.md` | generated from `bindings/python/mushroomdb.pyi`: every method signature and docstring, grouped as the stub groups them; exceptions as a table | generator `--check` in CI |
| `http.md` | written; one section per route, grouped as the router groups them, with auth and role-token behaviour stated once at the top | a test that reads the router's routes and fails on one the page does not name |
| `typescript.md` | written from `clients/typescript/README.md`; states which routes the client does not cover | none beyond the link check |
| `rust.md` | written overview: `GraphDb`, `SharedDb`, `ReaderSnapshot`, `BatchBuilder`, `OpenOptions`, `FsyncPolicy`, with one example each and links to docs.rs | the examples are doc-tested or lifted from a compiled example |

`api.md` is split into `http.md` and `python.md` and reduced to a few lines pointing at them,
so its URL keeps answering; `mcp.md` keeps its guide half
and loses its tool tables to `mcp-tools.md`. `crates/server/tests/doc_strings.rs` pins refusal
strings to pages by basename: its rows move with the text. The Concepts page and the Vector
search page are written in this pull request.

Done when: 29 of 29 routes, 25 of 25 tools, every method in the stub and every subcommand in
`usage()` appear in the reference, and removing one from a page fails CI.

## Pull request 4 — one voice

A short style section is added to `CONTRIBUTING.md` and every page is brought to it.

- Say what the product does now. A version number appears only where a reader must act on it
  (a format change, a migration). History lives in the changelog.
- Define a term before using it, or link to Concepts.
- No references a visitor cannot follow: section numbers of internal documents, audit names,
  "verified on <date>", source paths in running prose, test names.
- A limit is stated once, plainly, on the page it concerns and on Known limits — not hedged
  wherever the feature is mentioned.
- Benchmark pages give the current figure, its workload and its date. The run-by-run log moves
  to `benchmarks/`.
- Sentence-case headings; fenced code tagged `sh`, `python`, `rust`, `json`, `cypher` or
  `text`.
- The sentences to write toward are already in the repository: "Declare the rule. The edges
  maintain themselves." "A store is a directory. Mount it; everything below follows from
  that."

Structural edits in this pull request: the three quickstarts become one Quickstart and one
Install page; `multiprocess.md` folds into Concurrency; the repeated caveats (deletion is not
retroactive, keys are not identities, the MCP trust boundary, the lock model) are each stated
on one page and linked from the others.

Known contradictions to settle against the code: the snapshot default (V8 at `durability.md:3`,
V9 at `:118` and `api.md:1548`); "any 0.4.x release" at `durability.md:174`; the pre-alpha
notices at `views.md:123` and `rules.md:913`; "four" and "all three" algorithms in
`algorithms.md`; Python `None` in `suggest.md:46` against `api.md:524`; IVF-Flat at
`testing.md:246`; "The two rules" over nine at `ingest-git.md:117`; the `learn` pass at
`skill.md:93`; the hero image's alt text on the landing page.

The pitch is made the same everywhere it appears: the landing page, the README, `llms.txt`,
the social card, the crate and package descriptions, `server.json` and the registry blurbs
under `docs/registry-submissions/`.

Done when: a grep for `since 0\.|before v0\.|pre-alpha|Phase [0-9]` over `docs/site` returns
only lines a reader must act on; each listed contradiction is resolved with the code cited in
the pull request; all gates are green.

## Risks

- **mdBook's layout is fixed.** The palette, type and code style are ours; the three-pane
  layout is its own. If the result does not feel right after pull request 1, the Markdown is
  untouched and a different renderer is a contained change.
- **`llms-full.txt` changes in every pull request.** It is generated; the diff is noise to
  review. Each pull request regenerates it in its own final commit.
- **Renames break outside links.** `api.md` is the page most likely to be linked from
  elsewhere, which is why it stays as a pointer page. No other page is renamed; `multiprocess.md`
and `association-bench.md`, which fold into other pages, are left as pointers the same way.
- **Generated pages read flat.** A page made from `--help` is complete and dull. Each generated
  page takes hand-written context per group, kept in the generator's template.
