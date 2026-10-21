# Concepts

How mushroomdb works, in the order you meet it. Every command on this page runs against the
small graph `mushroomdb demo` writes, and the output shown is what it prints.

## A store is a directory

There is no server to start and nothing to connect to. A store is a directory, and a program
opens it.

```sh
mushroomdb demo ./db
mushroomdb stats ./db
```

```text
nodes: 60 live, 0 tombstoned
edges: 334
history: complete (nothing pruned)
rules: 7
```

`demo` wrote ten organisations, twenty projects, thirty people, and seven rules. It wrote none
of the 334 edges: the rules did.

Inside the directory, `wal.bin` is the log of every write. `snapshot.bin`, once you take a
snapshot, is the whole store as one file that opens without being read through. `LOCK` is how
processes take turns: one writer at a time, any number of readers, and that holds across
processes as well as inside one.

## Nodes

A node has a **key**, a **label** and **properties**. The key is unique across the whole store,
not per label. A node has exactly one label, fixed when it is created. A property is an integer,
a float, a string, a boolean, a list or a map.

In a query, a node's key is written as its `id` property: `CREATE (n:Person {id: 'ada'})`
makes the node whose key is `ada`, and `MATCH (p:Person {id: 'person-01'})` finds one by key.
`key(n)` returns it.

A node whose stored `id` property equals its key (what `CREATE` and the default ingest
produce) keeps it that way. On such a node, setting `id` to another value or removing `id` is
refused with `cannot change the key property `id` of node K; renaming a node is rename_node`,
whatever API the write comes from: Cypher, the Rust, Python and HTTP property calls, batches,
and upserts. Writing the key itself is accepted. A node whose `id` differs from its key, or
that has none (a store ingested with another `key_field`, such as `sku`, and an unrelated `id`
column), treats `id` as an ordinary property. `rename_node` changes a key and moves a matching
`id` with it.

Deleting a node **tombstones** it: it leaves the graph, and stays in the history.

## Edges

An edge has a **type**, a node it leaves and a node it reaches. It is directed, and there is at
most one of a type between the same two nodes in the same direction.

An edge gets there in one of two ways. You write it: a **hand-written** edge. Or a rule derives
it: a **derived** edge, which the rule owns. You cannot delete a derived edge yourself; the
store refuses. You change the data, or the rule, and the edge follows.

## Rules

A rule says which nodes should be linked. It names a source label, a destination label, a
**predicate** that compares a node of one with a node of the other, and the type of edge to
write when the predicate holds. There are six predicates:

| Predicate | Links two nodes when |
|---|---|
| `KeyMatch` | a field of one holds the other's key |
| `FieldEqual` | both hold the same value in a field |
| `Overlap` | their lists share enough items |
| `NumericWithin` | two numbers are within a tolerance |
| `GeoRadius` | two points are within a distance |
| `VectorSimilar` | two vectors are similar enough |

`All` and `Any` combine them. A rule can store the **score** of each match on its edge, and can
keep only the best few matches for each source node.

The demo's `skill_fit` rule links a person to every project whose skills overlap theirs by at
least half: the skills the two have in common, as a share of all the skills either has.

```sh
mushroomdb query ./db "MATCH (p:Person {id: 'person-01'})-[r:FIT]->(x:Project) RETURN x, r.score AS score ORDER BY score DESC, x"
```

```text
columns: x, score
x=proj-01  score=1.0
x=proj-02  score=0.5
x=proj-20  score=0.5
```

A rule is evaluated when it is created, against everything already in the store — the
**backfill** — and then on every insert and every property change, inside the write that made
the change. When a match stops holding, the rule **retracts** its edge in that same write.
Nothing is re-indexed afterwards and no job runs later. [Rules](rules.md) has each predicate
in full.

## Why two things are related

Every derived edge can say where it came from.

```sh
mushroomdb why ./db person-01 proj-01
```

```text
mushroomdb explain — person-01 ↔ proj-01: 2 relationship(s)
PROJECT via rule auto_fk_person_project_id (score 1.00) — key_match on project_id [project_id: proj-01]
FIT via rule skill_fit (score 1.00) — overlap on skills >= 0.5 [skills: s01, s02, s03]
```

Each line is one edge: its type, the rule that derived it, the score, the predicate, and in
brackets the **evidence** — the values the two nodes actually share. An assistant asked "why
are these two related?" can quote that instead of guessing.

Now change the person's skills, and ask again:

```sh
mushroomdb query ./db "MATCH (p:Person {id: 'person-01'}) SET p.skills = ['s20', 's21', 's22']"
mushroomdb why ./db person-01 proj-01
```

```text
mushroomdb explain — person-01 ↔ proj-01: 1 relationship(s)
PROJECT via rule auto_fk_person_project_id (score 1.00) — key_match on project_id [project_id: proj-01]
```

The `FIT` edge is gone. Nobody deleted it. The one `SET` made it untrue, and the rule retracted
it before the write returned.

## Time

A write does not replace what was there. It is appended to the log as a **commit**, and commits
are numbered from 0. So the store can be read as it was at any commit it still has — or on a
date, which resolves to the last commit at or before it.

The demo was 26 commits, 0 to 25. The `SET` above came after them, as two: the write itself,
and a record of what the rules did because of it. Read the graph as of commit 25, and the edge
is there:

```sh
mushroomdb asof ./db --commit 25 --query "MATCH (p:Person {id: 'person-01'})-[r:FIT]->(x:Project) RETURN x, r.score AS score ORDER BY score DESC, x"
```

```text
as-of commit 25 of 28
x=proj-01  score=1.0
x=proj-02  score=0.5
x=proj-20  score=0.5
```

History can be pruned, and a store says so when it has been. The oldest commit it still holds
is its **floor**; a read from before the floor is refused by name, never answered with a guess.
A commit number is never used twice: what is written after a prune is numbered on from where
the discarded history ended.
[Time travel](timetravel.md) has the other ways to ask: what edges a node had on a date,
whether two nodes were linked then, and every change to one node.

One thing that looks like time travel and is not: **what-if** computes the edges a property
change would gain and lose, and writes nothing.

## Who can see what

One store can answer different callers differently.

- A **namespace** is a tenancy boundary. A node is put in one when it is created and stays
  there; a node in none is in `default`. A hand-written edge cannot cross from one namespace to
  another.
- A **role** names what a caller may see — by label, by key, by a property test — and
  optionally what it may write. Roles are part of the store.
- A **mask** is a list of keys, for one call: only these.
- A **scope** binds a role, a namespace or a list of keys to a handle, so that every read
  through it obeys them. What a scope hides behaves exactly as if it did not exist.

Where this is enforced matters. Over HTTP, a role-bound token is held to its role by the
server. Over MCP and inside your own process, a role or a mask is a way of asking to be
answered as someone, and nothing stops a caller asking for more.
[Access control](masks.md) has the whole model.

## Memory

On top of the graph is a small surface for an assistant's memory, and it is how most people
meet mushroomdb.

- **`remember`** writes a note, the entities it names, and facts among them, in one commit.
  An entity it has never heard of is created as a **provisional** one — label `Entity`, marked
  as provisional — rather than refused. The mark is cleared when something describes it.
- **`recall`** takes a question in ordinary words and returns what the store holds about it.
- **`forget`** removes a node, a property or a hand-written fact from the graph. It does not
  erase history: that takes pruning.

Entities are ordinary nodes — `Person`, `Org`, `Project`, `Concept`, `Event` — and notes are
`Note` nodes linked to what they are about. So rules apply to memory as they do to anything
else: when enough entities share a field, the store can propose a rule that links them, and
once you accept it every later `remember` maintains those links. [Agent memory](mcp.md) walks through a session.

## Search

Three ways to find a node without knowing its key.

- **Full-text.** You declare which fields are searchable, by label and field. `recall` and
  `search` rank matches in them.
- **Vector.** `find_similar` returns the nodes whose vector is nearest a query vector. You
  supply the vectors; the store does not compute them. [Vector search](vector.md)
- **Hybrid.** Both at once, fused into one ranking.

## Words the other pages use

| Word | Meaning |
|---|---|
| WAL | The write-ahead log, `wal.bin`: every commit since the last snapshot |
| commit | One write, as appended to the log; numbered from 0 |
| snapshot | The whole store written as one file, so that opening it does not replay the log |
| archive | A stretch of the log set aside by a snapshot, so that history still reaches it |
| floor, horizon | The oldest commit a store still holds; 0 when nothing has been pruned |
| backfill | A new rule's first evaluation, against what is already in the store |
| fires, retracts | A rule derives an edge; a rule removes one that stopped being true |
| tripped | Of a rule: it reached its ceiling on derived edges and stopped adding more |
| auto-FK | A `KeyMatch` rule made for you at ingest, when a field ending in `_id` holds another label's keys |
| top-k, `max_edges` | Keep only the k best-scoring edges for each source node |
| task tools, graph tools | Over MCP: the task tools that answer a question in prose, and the graph tools that reach the store directly |
| the brief | The store's schema in one block, given to an assistant when a session starts |
| the digest | The short text a task tool answers with |
| provisional | Of an entity: created because something mentioned it, before anything described it |
| tombstone | A deleted node: out of the graph, still in the history |
| never-widen | A mask, a role and a scope combine by intersection: adding one can only hide more |
| approximate | Of a vector rule: allowed to miss some neighbours for speed. A search uses an index only on a field such a rule covers |
