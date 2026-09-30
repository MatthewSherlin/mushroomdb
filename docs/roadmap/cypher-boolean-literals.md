# The Cypher subset has no boolean literals

**Filed 2026-09-30. Pre-existing; not introduced by any v0.7 work.**

Filed as its own document rather than inside a release's leftovers list, deliberately: this is a
query-language defect with no relationship to the memory write path, and folding it into
"v0.7 plan 1 leftovers" is how it would stay invisible for another three releases.

## What happens

`WHERE <prop> = true` — or `= false` — errors on any `Bool` property:

```
MATCH (n) WHERE n.provisional = true RETURN n
  → execute: unbound variable 'true' in WHERE
```

The parser has no `Bool` arm. `crates/core-query/src/cypher/parser.rs:911-926` and `:1286-1310`
emit only `Value::Int` and `Value::Str` for literals, so `true` falls through to identifier
resolution and is reported as an unbound variable.

## What works instead

The bare truthy form:

```
MATCH (n) WHERE n.provisional RETURN n
```

This is correct, not a lucky accident — it was verified on a three-node probe to discriminate
`true`, `false` and absent properly, returning only the node whose flag is `true`.

## Why it needs finding now

It has acquired a second dependent. `crates/cli/tests/first_run.rs:160-165` uses the truthy form
and discloses why at the point of use, because listing provisional entities is the documented way
to see what `remember`'s provisional-subject path creates
(`docs/roadmap/v0.7-memory-write-path-spec.md` §3.2). So the workaround is now load-bearing in a
release gate, and the next person to touch the parser needs to find this note before they change
either side of it.

## Scope when someone picks it up

A `Bool` literal arm in the parser is the obvious fix, but check both call sites above — the
literal path and the parameter path diverge — and check whether `<>`/`!=` against a boolean needs
the same treatment. Reproduced independently by two agents during the v0.7 plan 1 review; neither
found any other affected form.
