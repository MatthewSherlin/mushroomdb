# Launch checklist

Working notes for the public launch. Not linked from `README.md`.

---

## One-time repository settings

- [ ] **Enable GitHub Pages.** Settings → Pages → Source: **GitHub Actions**. The
      `pages` workflow (`.github/workflows/pages.yml`) deploys `docs/site` plus a copy of
      `docs/assets` on every push to `main`; it cannot publish until Pages is enabled once
      by an owner.
- [ ] **Enable Discussions** if you want a Q&A surface. The README links line omits it
      today because the repository has Discussions turned off — add
      `· [Discussions](https://github.com/MatthewSherlin/mushroomdb/discussions)` to the
      links line under the badges once it is on.
- [ ] **Add the `good first issue` label** and file the five candidates below.
- [ ] **Set the repository description and topics** so the GitHub sidebar matches the
      README tagline: `graph-database`, `rust`, `embedded-database`, `mcp`, `agent-memory`.
- [ ] **Set the social preview image** (Settings → General → Social preview) from
      `docs/assets/social-preview.png`.

---

## `good first issue` candidates

Five real deferred items. Each is self-contained, has a test surface, and does not need
context from the release plan. File them as issues; do not batch them into one.

### 1. Render commit subjects in the recall digest

**File:** `crates/cli/src/recall.rs`

The digest line for each hit is `- <key> [<label>] <name>`. For a `Commit` node from
`ingest-git` that is a bare SHA, which tells the assistant nothing about the change.

**Acceptance:** a `Commit` node's digest line includes its subject property, passed
through the existing `sanitize` helper, and the added bytes still count against the
digest byte budget so a large store cannot blow past it.

### 2. Separate real deletes from rename evictions in the `ingest-git` summary

**File:** `crates/cli/src/ingest_git.rs`

`report.deleted` is incremented both when a file is deleted and when a rename's
destination did not survive the commit window and the node is evicted instead of moved.
The printed summary reports one number for two different events.

**Acceptance:** the report distinguishes the two (a new counter, or a split of the
existing one), the printed summary names both, and a test covers a rename whose
destination is later deleted.

### 3. Sanitise non-ASCII control characters in recall

**File:** `crates/cli/src/recall.rs`

`sanitize` replaces ASCII control characters only. Unicode bidirectional overrides
(`U+202E`), zero-width characters (`U+200B`), and the line/paragraph separators
(`U+2028`, `U+2029`) pass through untouched into the assistant's context, and some
renderers treat the separators as line breaks.

**Acceptance:** those categories are neutralised alongside the ASCII controls, the
one-char-in-one-char-out property that the byte budget relies on is preserved or the
budget accounting is updated to match, and a test covers each category.

### 4. Fix the stale `OpenOptions` snippet in the format-stability doc

**File:** `docs/format-stability.md`

The opt-out snippet reads `GraphDb::open_with_options(dir, OpenOptions { auto_migrate: false })`.
`OpenOptions` has carried a second field (`repair_wal`) since then, so the snippet as
printed does not compile.

**Acceptance:** the snippet compiles against the current struct (add `..Default::default()`
or the missing field) and any other struct-literal snippets in the same file are checked
the same way.

### 5. Support `CASE` in a write-statement `RETURN` projection

**File:** `crates/core-api/src/db.rs` (`eval_set_return_operand`, around line 736)

`CASE WHEN … THEN … END` works anywhere a scalar expression is allowed in a read query,
but the write-statement projection path (`CREATE`/`MERGE`/`SET … RETURN`) rejects it with
"CASE is not supported in a write-statement RETURN projection; use a read query". The read
path already has the evaluator.

**Acceptance:** `MATCH (n:Person) SET n.age = 30 RETURN CASE WHEN n.age >= 65 THEN 'senior'
ELSE 'other' END` returns a row instead of an error, the read and write paths agree on the
same inputs, and the coverage table in `docs/site/query.md` is updated.

---

## Before announcing

- [ ] Every command on the README first screen run from a clean machine against the
      published `0.6.0` artifacts, not a local build.
- [ ] Badges render on the rendered README (stars, crates.io, npm, PyPI, CI, license).
- [ ] Pages URL loads, GIFs included, in light and dark browser themes.
- [ ] `CHANGELOG.md` top entry matches the published tag.
