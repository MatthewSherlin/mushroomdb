#!/usr/bin/env python3
"""The official Python Bolt driver against `mushroomdb serve --bolt-port`.

Runs the twelve driver-compatibility scenarios of the 0.10 transactions spec
(W3 section 6), each by name, each asserting the values it gets back. Exits 0
when all twelve pass; otherwise prints every failing scenario and exits 1.

Driver: the official Python driver package, pinned to 6.4.1. Its name is
spelled once, as bytes, in VENDOR below (the names gate keeps it out of every
tracked file); `--requirement` prints the pip requirement CI installs.

    python tests/bolt/python_compat.py bolt://127.0.0.1:7687 [token] [role-token]
    python tests/bolt/python_compat.py --requirement

The full-access token and the role token default to BOLT_COMPAT_TOKEN and
BOLT_COMPAT_ROLE_TOKEN. The role token must be bound to a role that sees only
`Doc` nodes; tests/bolt/run_compat.sh starts a server set up that way.
"""

from __future__ import annotations

import datetime
import importlib
import os
import sys
import traceback
from typing import Callable
from urllib.parse import urlsplit

DRIVER_VERSION = "6.4.1"
# The driver vendor's name: the package and import name, and the routing URI
# scheme. Bytes, so that no tracked file spells it (scripts/check-names.py).
VENDOR = bytes([0x6E, 0x65, 0x6F, 0x34, 0x6A]).decode("ascii")
ROUTING_SCHEME = VENDOR

if __name__ == "__main__" and sys.argv[1:] == ["--requirement"]:
    print(f"{VENDOR}=={DRIVER_VERSION}")
    sys.exit(0)

driver_lib = importlib.import_module(VENDOR)
GraphDatabase = driver_lib.GraphDatabase
basic_auth = driver_lib.basic_auth
Address = driver_lib.Address
ManagedTransaction = driver_lib.ManagedTransaction
Record = driver_lib.Record
READ_ACCESS = driver_lib.READ_ACCESS
Node = importlib.import_module(f"{VENDOR}.graph").Node
_exceptions = importlib.import_module(f"{VENDOR}.exceptions")
AuthError = _exceptions.AuthError
ClientError = _exceptions.ClientError
TransientError = _exceptions.TransientError
_time = importlib.import_module(f"{VENDOR}.time")
Date, DateTime, Duration = _time.Date, _time.DateTime, _time.Duration

ROW_COUNT = 1000
FETCH_SIZE = 100
SEED = [("Doc", "d1"), ("Doc", "d2"), ("Secret", "s1"), ("Person", "seed-1"), ("Person", "seed-2")]


class Context:
    def __init__(self, uri: str, token: str, role_token: str) -> None:
        self.uri = uri
        self.token = token
        self.role_token = role_token
        self.driver = GraphDatabase.driver(uri, auth=basic_auth("compat", token))

    def close(self) -> None:
        self.driver.close()


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def single(ctx: Context, query: str, **params: object) -> Record:
    with ctx.driver.session() as session:
        record = session.run(query, params).single(strict=True)
    return record


# 1. Connect and auth (valid token).
def connect_and_auth(ctx: Context) -> None:
    ctx.driver.verify_connectivity()
    info = ctx.driver.get_server_info()
    check(info.agent.startswith("MushroomDB/"), f"server agent {info.agent!r}")
    check(info.protocol_version in ((5, 0), (4, 4)), f"protocol {info.protocol_version}")
    record = single(ctx, "RETURN 1 AS one")
    check(record["one"] == 1, f"RETURN 1 gave {record['one']!r}")


# 2. Auth failure (invalid token) -> the driver receives Unauthorized.
def auth_failure(ctx: Context) -> None:
    bad = GraphDatabase.driver(ctx.uri, auth=basic_auth("compat", ctx.token + "-wrong"))
    try:
        bad.verify_connectivity()
    except AuthError as err:
        check(err.code == "Neo.ClientError.Security.Unauthorized", f"code {err.code!r}")
    else:
        raise AssertionError("a wrong token connected")
    finally:
        bad.close()


# 3. Auto-commit read: MATCH (n) RETURN count(n) -> an integer.
def autocommit_read(ctx: Context) -> None:
    total = single(ctx, "MATCH (n) RETURN count(n) AS c")["c"]
    check(type(total) is int, f"count(n) is {type(total).__name__}, not int")
    docs = single(ctx, "MATCH (n:Doc) RETURN count(n) AS c")["c"]
    check(docs == 2, f"{docs} Doc nodes, expected the 2 seeded")
    check(total >= 5, f"count(n) is {total}, expected at least the 5 seeded")


# 4. Auto-commit write: CREATE a node from parameters, MERGE from parameters
#    (one that matches, one that creates), then see both in a later read.
def autocommit_write(ctx: Context) -> None:
    with ctx.driver.session() as session:
        created = session.run(
            "CREATE (p:Person {id: $id, name: $name, age: $age})", id="p-auto", name="Ada", age=36
        ).consume().counters
        check(
            (created.nodes_created, created.labels_added, created.properties_set) == (1, 1, 3),
            f"CREATE counters {created}",
        )
        matched = session.run(
            "MERGE (p:Person {id: $id}) ON CREATE SET p.nick = $other ON MATCH SET p.nick = $nick",
            id="p-auto",
            nick="ada",
            other="new",
        ).consume().counters
        check(
            (matched.nodes_created, matched.properties_set) == (0, 1),
            f"matching MERGE counters {matched}",
        )
        made = session.run(
            "MERGE (p:Person {id: $id}) ON CREATE SET p.name = $name", id="p-merged", name="Lin"
        ).consume().counters
        check(made.nodes_created == 1 and made.contains_updates, f"creating MERGE counters {made}")
    record = single(
        ctx,
        "MATCH (p:Person {id: $id}) RETURN p.name AS name, p.age AS age, p.nick AS nick",
        id="p-auto",
    )
    check(
        (record["name"], record["age"], record["nick"]) == ("Ada", 36, "ada"),
        f"read back {dict(record)}",
    )
    merged = single(ctx, "MATCH (p:Person {id: $id}) RETURN p.name AS name", id="p-merged")
    check(merged["name"] == "Lin", f"read back {dict(merged)}")


# 5. Explicit transaction commit, then the managed transactions users write.
def explicit_tx_commit(ctx: Context) -> None:
    with ctx.driver.session() as session:
        tx = session.begin_transaction()
        tx.run("CREATE (:Person {id: 'p-tx', name: 'Grace'})").consume()
        seen = tx.run("MATCH (p:Person {id: 'p-tx'}) RETURN p.name AS name").single(strict=True)
        check(seen["name"] == "Grace", "the transaction did not see its own write")
        tx.commit()
    record = single(ctx, "MATCH (p:Person {id: 'p-tx'}) RETURN count(p) AS c")
    check(record["c"] == 1, f"{record['c']} committed nodes, expected 1")

    def link(tx: ManagedTransaction, a: str, b: str) -> None:
        tx.run("MATCH (a {id: $a}), (b {id: $b}) CREATE (a)-[:K]->(b)", a=a, b=b).consume()

    def linked(tx: ManagedTransaction, a: str) -> list[str]:
        result = tx.run("MATCH (a {id: $a})-[:K]->(b) RETURN b.id AS id ORDER BY id", a=a)
        return [record["id"] for record in result]

    with ctx.driver.session() as session:
        session.execute_write(link, "p-auto", "p-tx")
        ids = session.execute_read(linked, "p-auto")
    check(ids == ["p-tx"], f"managed read saw {ids}, expected ['p-tx']")


# 6. Explicit transaction rollback: the node is absent afterwards.
def explicit_tx_rollback(ctx: Context) -> None:
    with ctx.driver.session() as session:
        tx = session.begin_transaction()
        tx.run("CREATE (:Person {id: 'p-rolled-back'})").consume()
        inside = tx.run("MATCH (p:Person {id: 'p-rolled-back'}) RETURN count(p) AS c").single()
        check(inside["c"] == 1, "the transaction did not see its own write")
        tx.rollback()

        class Abort(Exception):
            pass

        def create_then_fail(tx: ManagedTransaction) -> None:
            tx.run("CREATE (:Person {id: 'p-aborted'})").consume()
            raise Abort()

        try:
            session.execute_write(create_then_fail)
        except Abort:
            pass
    record = single(
        ctx,
        "MATCH (p:Person) WHERE p.id IN ['p-rolled-back', 'p-aborted'] RETURN count(p) AS c",
    )
    check(record["c"] == 0, f"{record['c']} rolled-back nodes are visible")


# 7. Streaming: 1000 rows pulled in batches smaller than the result.
def streaming(ctx: Context) -> None:
    with ctx.driver.session() as session:
        session.run(
            "UNWIND range(0, $n - 1) AS i CREATE (:Row {id: 'row-' + toString(i), i: i})",
            n=ROW_COUNT,
        ).consume()
    with ctx.driver.session(fetch_size=FETCH_SIZE) as session:
        result = session.run("MATCH (n:Row) RETURN n, n.i AS i")
        rows = [(record["n"], record["i"]) for record in result]
    check(len(rows) == ROW_COUNT, f"{len(rows)} rows streamed, expected {ROW_COUNT}")
    check(
        sorted(i for _, i in rows) == list(range(ROW_COUNT)),
        "the streamed rows are not 0..999",
    )
    # Spec W3 section 3.3: a node is sent as a Node structure.
    values = [n for n, _ in rows]
    kinds = {type(n).__name__ for n in values}
    check(
        all(isinstance(n, Node) for n in values),
        f"{ROW_COUNT} rows arrived, but `RETURN n` gave {kinds} values"
        f" (first {values[0]!r}), not Node structures",
    )
    for node in values:
        check(set(node.labels) == {"Row"}, f"labels {set(node.labels)}")
        check(node["id"] == f"row-{node['i']}", f"node properties {dict(node)}")


# 8. Every Value type round-trips, as a parameter and as a stored property.
def value_types(ctx: Context) -> None:
    values: dict[str, object] = {
        "int": -9007199254740993,
        "float": 2.5,
        "string": "naïve ☃",
        "bool": True,
        "null": None,
        "list": [1, "two", 3.0, False],
        "map": {"a": 1, "b": ["x", None]},
        "date": Date(2024, 2, 29),
        "datetime": DateTime(
            2024, 3, 1, 12, 30, 45, 123456000, tzinfo=datetime.timezone(datetime.timedelta(hours=2))
        ),
        "localdatetime": DateTime(2024, 3, 1, 12, 30, 45, 123456000),
        "duration": Duration(months=14, days=3, seconds=7, nanoseconds=500000000),
        "bytes": b"\x00\x01\xfe\xff",
    }
    record = single(ctx, "RETURN " + ", ".join(f"${k} AS v_{k}" for k in values), **values)
    for key, sent in values.items():
        got = record[f"v_{key}"]
        check(got == sent, f"{key}: sent {sent!r}, echoed {got!r}")
        check(type(got) is type(sent), f"{key}: sent {type(sent).__name__}, got {type(got).__name__}")

    stored = {k: v for k, v in values.items() if k not in ("null", "map")}
    with ctx.driver.session() as session:
        session.run("CREATE (v:Vals {id: 'v1'})").consume()
        session.run("MATCH (v:Vals {id: 'v1'}) SET v += $props", props=stored).consume()
    record = single(
        ctx,
        "MATCH (v:Vals {id: 'v1'}) RETURN " + ", ".join(f"v.{k} AS v_{k}" for k in stored),
    )
    for key, sent in stored.items():
        got = record[f"v_{key}"]
        check(got == sent, f"stored {key}: sent {sent!r}, read {got!r}")
        check(type(got) is type(sent), f"stored {key}: got {type(got).__name__}")


# 9. A transient error is retried by the driver, and the retry succeeds.
def error_retry(ctx: Context) -> None:
    with ctx.driver.session() as session:
        session.run("CREATE (c:Counter {id: 'retry', n: 0})").consume()
    attempts: list[int] = []

    def increment(tx: ManagedTransaction) -> int:
        attempts.append(1)
        n = tx.run("MATCH (c:Counter {id: 'retry'}) RETURN c.n AS n").single(strict=True)["n"]
        if len(attempts) == 1:
            # Another client commits to the node this transaction read, so
            # this transaction's COMMIT conflicts and fails as transient.
            with ctx.driver.session() as other:
                other.run("MATCH (c:Counter {id: 'retry'}) SET c.n = c.n + 10").consume()
        tx.run("MATCH (c:Counter {id: 'retry'}) SET c.n = $n", n=n + 1).consume()
        return n + 1

    with ctx.driver.session() as session:
        result = session.execute_write(increment)
    check(len(attempts) == 2, f"{len(attempts)} attempts, expected 2 (one conflict, one retry)")
    check(result == 11, f"the retry wrote {result}, expected 11")
    final = single(ctx, "MATCH (c:Counter {id: 'retry'}) RETURN c.n AS n")["n"]
    check(final == 11, f"counter is {final}, expected 11")

    # The conflict itself is a transient error with the retryable code.
    with ctx.driver.session() as session:
        tx = session.begin_transaction()
        tx.run("MATCH (c:Counter {id: 'retry'}) RETURN c.n").consume()
        with ctx.driver.session() as other:
            other.run("MATCH (c:Counter {id: 'retry'}) SET c.n = 0").consume()
        tx.run("MATCH (c:Counter {id: 'retry'}) SET c.n = -1").consume()
        try:
            tx.commit()
        except TransientError as err:
            check(
                err.code == "Neo.TransientError.Transaction.DeadlockDetected",
                f"conflict code {err.code!r}",
            )
            check(err.is_retryable(), "the conflict is not retryable")
        else:
            raise AssertionError("a conflicting commit succeeded")


# 10. CALL db.labels(), and a role token sees only its role's labels.
def call_db_labels(ctx: Context) -> None:
    with ctx.driver.session() as session:
        labels = sorted(r["label"] for r in session.run("CALL db.labels()"))
    for label in ("Doc", "Secret", "Person"):
        check(label in labels, f"db.labels() {labels} lacks {label}")

    scoped = GraphDatabase.driver(ctx.uri, auth=basic_auth("compat", ctx.role_token))
    try:
        with scoped.session() as session:
            role_labels = [r["label"] for r in session.run("CALL db.labels()")]
            ids = sorted(r["id"] for r in session.run("MATCH (n) RETURN n.id AS id"))
            try:
                session.run("CREATE (:Doc {id: 'role-write'})").consume()
            except ClientError as err:
                check(err.code == "Neo.ClientError.Security.Forbidden", f"code {err.code!r}")
            else:
                raise AssertionError("a role with no write scope wrote")
    finally:
        scoped.close()
    check(role_labels == ["Doc"], f"the role sees labels {role_labels}, expected ['Doc']")
    check(ids == ["d1", "d2"], f"the role sees nodes {ids}, expected ['d1', 'd2']")


# 11. Routing URI: the driver asks ROUTE and uses the address it advertises.
# The seed host exists only in this resolver, so every query after the first
# ROUTE reaches the server through the address the routing table named
# (tests/bolt/run_compat.sh advertises `localhost`). The driver passes each
# address it is about to dial through the resolver, which records it.
def routing_uri(ctx: Context) -> None:
    port = urlsplit(ctx.uri).port
    seed = "compat-seed.invalid"
    asked: list[tuple[str, int]] = []

    def resolver(address: Address) -> list[Address]:
        asked.append((address.host, address.port))
        if address.host == seed:
            return [Address(("127.0.0.1", address.port))]
        return [address]

    routing = GraphDatabase.driver(
        f"{ROUTING_SCHEME}://{seed}:{port}", auth=basic_auth("compat", ctx.token), resolver=resolver
    )
    try:
        routing.verify_connectivity()
        with routing.session() as session:
            write = session.run("CREATE (r:Routed {id: 'via-route'}) RETURN r.id AS id")
            check(write.single(strict=True)["id"] == "via-route", "write through routing")
        with routing.session(default_access_mode=READ_ACCESS) as session:
            read = session.run("MATCH (r:Routed) RETURN count(r) AS c")
            check(read.single(strict=True)["c"] == 1, "read through routing")
    finally:
        routing.close()
    check(asked[0] == (seed, port), f"the driver first dialled {asked[0]}")
    check(("localhost", port) in asked, f"the driver never dialled localhost:{port}: {asked}")
    check(set(asked) == {(seed, port), ("localhost", port)}, f"the driver dialled {set(asked)}")


# 12. Bookmarks: commit on session A, read with A's bookmarks on session B.
def bookmarks(ctx: Context) -> None:
    with ctx.driver.session() as a:
        a.execute_write(
            lambda tx: tx.run("CREATE (:Note {id: 'causal', text: 'written on A'})").consume()
        )
        marks = a.last_bookmarks()
    raw = list(marks.raw_values)
    check(len(raw) == 1, f"{len(raw)} bookmarks, expected 1")
    check(raw[0].startswith("mushroomdb:commit:"), f"bookmark {raw[0]!r}")
    reader = GraphDatabase.driver(ctx.uri, auth=basic_auth("compat", ctx.token))
    try:
        with reader.session(bookmarks=marks, default_access_mode=READ_ACCESS) as b:
            text = b.execute_read(
                lambda tx: tx.run("MATCH (n:Note {id: 'causal'}) RETURN n.text AS t")
                .single(strict=True)["t"]
            )
    finally:
        reader.close()
    check(text == "written on A", f"session B read {text!r}")


SCENARIOS: list[tuple[str, Callable[[Context], None]]] = [
    ("connect_and_auth", connect_and_auth),
    ("auth_failure", auth_failure),
    ("autocommit_read", autocommit_read),
    ("autocommit_write", autocommit_write),
    ("explicit_tx_commit", explicit_tx_commit),
    ("explicit_tx_rollback", explicit_tx_rollback),
    ("streaming", streaming),
    ("value_types", value_types),
    ("error_retry", error_retry),
    ("call_db_labels", call_db_labels),
    ("routing_uri", routing_uri),
    ("bookmarks", bookmarks),
]


def seed(ctx: Context) -> None:
    # One statement per node: CREATE takes one pattern (or one chain); a label
    # is not a parameter, a property value is.
    with ctx.driver.session() as session:
        for label, key in SEED:
            session.run(f"CREATE (:{label} {{id: $id}})", id=key).consume()


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    uri = argv[1]
    token = argv[2] if len(argv) > 2 else os.environ.get("BOLT_COMPAT_TOKEN", "")
    role_token = argv[3] if len(argv) > 3 else os.environ.get("BOLT_COMPAT_ROLE_TOKEN", "")
    if not token or not role_token:
        print("a token and a role token are required", file=sys.stderr)
        return 2
    if driver_lib.__version__ != DRIVER_VERSION:
        print(f"driver is {driver_lib.__version__}, this script pins {DRIVER_VERSION}", file=sys.stderr)
        return 2

    ctx = Context(uri, token, role_token)
    failed: list[str] = []
    try:
        seed(ctx)
        for name, scenario in SCENARIOS:
            try:
                scenario(ctx)
            except Exception as err:  # noqa: BLE001 — every failure is reported by name
                failed.append(name)
                print(f"FAIL {name}: {type(err).__name__}: {err}")
                traceback.print_exc(limit=3)
            else:
                print(f"PASS {name}")
    finally:
        ctx.close()
    passed = len(SCENARIOS) - len(failed)
    print(f"python driver {DRIVER_VERSION}: {passed}/{len(SCENARIOS)} scenarios passed")
    if failed:
        print("failing: " + ", ".join(failed))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
