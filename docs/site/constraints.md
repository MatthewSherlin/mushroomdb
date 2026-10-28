# Unique constraints

A unique constraint prevents two live nodes of one label from receiving the
same value for one property. Missing properties do not conflict. Values use
query equality: for example, integer `2` and float `2.0` conflict, as do two
date-times at the same UTC instant. Lists, maps and Duration have no equality
index key and do not conflict under this constraint.

## Declare and inspect

Names are required for constraints. Only one property and `IS UNIQUE` are
supported; composite, existence and node-key constraints are outside 0.10.

```sh
mushroomdb query constraint-demo "CREATE CONSTRAINT uniq_email FOR (u:User) REQUIRE u.email IS UNIQUE"
mushroomdb schema constraints constraint-demo
```

```text
name       | type   | label | property
uniq_email | unique | User  | email
```

The same declarations can be written directly in Cypher:

```cypher
CREATE CONSTRAINT uniq_email IF NOT EXISTS FOR (u:User) REQUIRE u.email IS UNIQUE
SHOW CONSTRAINTS
```

`SHOW CONSTRAINTS` returns `name`, `type` (`unique`), `label` and `property`,
sorted by name. A schema-changing statement is a whole statement and returns
no columns or rows; it cannot be followed by `RETURN`. `IF NOT EXISTS` makes
creation a no-op when the constraint name or label/property pair exists.
`IF EXISTS` makes dropping a missing constraint a no-op.

Declaring a constraint creates its backing equality index when needed.
`SHOW INDEXES` lists it as `eq_User_email`. Dropping that index while owned by
a constraint is refused. Dropping the constraint leaves the equality index
in place; it can then be dropped independently.

The first constraint declaration opts the store into V11 before its WAL
record is appended. Dropping it does not downgrade the store. See
[format stability](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/format-stability.md#v11-is-opt-in-and-sticky).

## Writes and failures

```sh
mushroomdb query constraint-demo "CREATE (:User {id:'ada', email:'ada@example.test'})"
```

Trying to create another User with that email refuses the write:

```cypher
CREATE (:User {id:'other', email:'ada@example.test'})
```

```text
constraint violation: property 'email' is declared unique by constraint 'uniq_email'; a node with that value already exists
```

The check runs before WAL append for new single and batch writes, transaction
COMMIT, MERGE and CSV import. A duplicate within one batch refuses that whole
batch. Import batches that committed before a later failing batch remain;
an import is not one transaction over the whole file. Actual writes generated
by rules pass through the same enforcement; declaring a constraint does not
give rules a new arbitrary property-write operation.

A creation scans existing nodes of the label. If they already violate the
constraint, `ConstraintCreationConflict` reports up to the first **100
conflicting nodes**, as `(node_key, value)` pairs in node-id order, and a
`total` count of all conflicting nodes. Nothing is declared and data is
unchanged. The scan and index build block writers in proportion to label
size; readers retain their snapshots.

## Visibility and transactions

Withdraw the constraint when it is no longer needed:

```cypher
DROP CONSTRAINT uniq_email IF EXISTS
```

Uniqueness is store-global, including nodes hidden from a caller. A write
conflict names the property and constraint, without the conflicting key or a
hidden/visible status. A caller can still probe whether a value exists by
observing which writes are refused. This existence-probing tradeoff is part
of the global uniqueness contract.

Schema changes need full write access. They are refused in transactions,
read-only/as-of handles, masked or namespaced reads and role-bound sessions.
Existing role/read-tier transactions remain read-only; uniqueness does not
add authorization-aware writes. `SHOW` is a read: scoped callers see a schema
row only when they can see a node of its label; full-access reads see all rows.

Transactions still use snapshot isolation, not serializable predicate locks.
See [Transactions](transactions.md) for concurrent writes and phantom limits.

## Replay and historical duplicates

Open, WAL replay, refresh and `open_at` load historical records without
re-enforcing uniqueness. A legacy history containing duplicates can open.
New writes are checked only for constrained values they change: changing
another property or writing the same duplicate value again is allowed;
assigning a different value that another live node holds is refused. Resolve
old duplicates by deleting nodes or assigning distinct values.

See [Cypher DDL](query.md#schema-statements-ddl), [CLI schema commands](cli.md#schema)
and [HTTP schema routes](http.md#schema-indexes-and-constraints) for the supported
interfaces. HTTP `name`, `label` and `property` are required; `type` is `unique`.
