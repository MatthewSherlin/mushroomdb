"""Interactive transactions (0.10 TX-2b): `with db.transaction() as tx:` and
`db.begin_transaction()`, over the engine's `Transaction`.

The handle keeps the cross-process write lock for its whole life, so the first
transaction wraps the loaded store in the engine's shared handle without giving
that lock up; these tests pin both halves — the transaction semantics and the
handle behaving as before once it has been wrapped.
"""

from __future__ import annotations

import gc
import threading

import pytest

import mushroomdb
from mushroomdb import GraphDb

_JOIN_SECONDS = 120


@pytest.fixture
def db(tmp_path):
    handle = GraphDb.open(str(tmp_path / "store"))
    yield handle
    try:
        handle.close()
    except RuntimeError:
        pass


def _ids(handle) -> list[str]:
    return sorted(r["id"] for r in handle.query("MATCH (n:P) RETURN n.id AS id"))


def _reader(db_path: str) -> GraphDb:
    return GraphDb.open(db_path, read_only=True)


# -- commit / rollback -------------------------------------------------------


def test_with_block_commits_on_clean_exit(db):
    with db.transaction() as tx:
        tx.execute("CREATE (n:P {id: 'a'})")
        tx.execute("CREATE (n:P {id: 'b'})")
    assert _ids(db) == ["a", "b"]


def test_with_block_rolls_back_on_exception_and_reraises(db):
    with pytest.raises(ZeroDivisionError):
        with db.transaction() as tx:
            tx.execute("CREATE (n:P {id: 'a'})")
            1 / 0
    assert _ids(db) == []


def test_rollback_leaves_no_trace_in_the_history(db):
    before = db.stats()
    tx = db.begin_transaction()
    tx.execute("CREATE (n:P {id: 'a'})")
    tx.rollback()
    assert _ids(db) == []
    assert db.stats()["nodes_live"] == before["nodes_live"]


def test_commit_returns_a_commit_number(db):
    tx = db.begin_transaction()
    tx.execute("CREATE (n:P {id: 'a'})")
    first = tx.commit()
    assert isinstance(first, int)
    tx = db.begin_transaction()
    tx.execute("CREATE (n:P {id: 'b'})")
    assert tx.commit() > first


def test_a_transaction_that_wrote_nothing_commits_without_a_frame(db):
    db.insert_node("P", "seed", {"id": "seed"})
    tx = db.begin_transaction()
    assert tx.execute("MATCH (n:P) RETURN n.id AS id") == [{"id": "seed"}]
    number = tx.commit()
    tx = db.begin_transaction()
    assert tx.commit() == number


def test_explicit_commit_inside_with_is_not_repeated_on_exit(db):
    with db.transaction() as tx:
        tx.execute("CREATE (n:P {id: 'a'})")
        tx.commit()
    assert _ids(db) == ["a"]


def test_explicit_rollback_inside_with_survives_clean_exit(db):
    with db.transaction() as tx:
        tx.execute("CREATE (n:P {id: 'a'})")
        tx.rollback()
    assert _ids(db) == []


# -- isolation ---------------------------------------------------------------


def test_a_transaction_reads_its_own_writes(db):
    db.insert_node("P", "seed", {"id": "seed"})
    with db.transaction() as tx:
        tx.execute("CREATE (n:P {id: 'a'})")
        rows = tx.execute("MATCH (n:P) RETURN n.id AS id")
        assert sorted(r["id"] for r in rows) == ["a", "seed"]


def test_writes_are_invisible_to_other_handles_until_commit(tmp_path, db):
    path = str(tmp_path / "store")
    db.insert_node("P", "seed", {"id": "seed"})
    reader = _reader(path)
    try:
        tx = db.begin_transaction()
        tx.execute("CREATE (n:P {id: 'a'})")
        assert db.query("MATCH (n:P) RETURN n.id AS id") == [{"id": "seed"}]
        reader.refresh()
        assert _ids(reader) == ["seed"]
        tx.commit()
        reader.refresh()
        assert _ids(reader) == ["a", "seed"]
    finally:
        reader.close()


def test_execute_takes_params_and_returns_query_shaped_rows(db):
    with db.transaction() as tx:
        counts = tx.execute("CREATE (n:P {id: $id, age: $age})", {"id": "a", "age": 3})
        assert counts[0]["created"] == 1
        rows = tx.execute(
            "MATCH (n:P) WHERE n.age > $min RETURN n.id AS id", {"min": 1}
        )
        assert rows == [{"id": "a"}]
    assert db.query("MATCH (n:P) RETURN n.id AS id") == [{"id": "a"}]


def test_a_failed_statement_leaves_the_transaction_usable(db):
    with db.transaction() as tx:
        tx.execute("CREATE (n:P {id: 'a'})")
        with pytest.raises(mushroomdb.QueryError):
            tx.execute("THIS IS NOT CYPHER")
        tx.execute("CREATE (n:P {id: 'b'})")
    assert _ids(db) == ["a", "b"]


def test_a_transaction_snapshot_ignores_later_handle_writes(db):
    tx = db.begin_transaction()
    db.insert_node("P", "late", {"id": "late"})
    assert tx.execute("MATCH (n:P) RETURN n.id AS id") == []
    tx.rollback()


# -- finished transactions ---------------------------------------------------


@pytest.mark.parametrize("finish", ["commit", "rollback"])
@pytest.mark.parametrize("use", ["execute", "commit", "rollback"])
def test_using_a_finished_transaction_raises(db, finish, use):
    tx = db.begin_transaction()
    getattr(tx, finish)()
    with pytest.raises(mushroomdb.TransactionClosed) as caught:
        if use == "execute":
            tx.execute("MATCH (n:P) RETURN n.id AS id")
        else:
            getattr(tx, use)()
    assert isinstance(caught.value, mushroomdb.MushroomError)
    assert caught.value.code == "transaction_closed"
    assert "finished" in str(caught.value)


# -- conflicts ---------------------------------------------------------------


def test_a_commit_conflict_raises_a_retryable_class(db):
    db.insert_node("P", "acct", {"id": "acct", "n": 0})
    first = db.begin_transaction()
    second = db.begin_transaction()
    first.execute("MATCH (n:P {id: 'acct'}) SET n.n = 1")
    second.execute("MATCH (n:P {id: 'acct'}) SET n.n = 2")
    first.commit()
    with pytest.raises(mushroomdb.TransactionConflict) as caught:
        second.commit()
    conflict = caught.value
    assert isinstance(conflict, mushroomdb.CasConflict)
    assert conflict.code == "transaction_conflict"
    assert conflict.key == "acct"
    # The loser is finished; the retry is a fresh transaction.
    with pytest.raises(mushroomdb.TransactionClosed):
        second.commit()
    with db.transaction() as retry:
        retry.execute("MATCH (n:P {id: 'acct'}) SET n.n = 2")
    assert db.query("MATCH (n:P) RETURN n.n AS n") == [{"n": 2}]


def test_a_conflict_on_with_exit_propagates_and_writes_nothing(db):
    db.insert_node("P", "acct", {"id": "acct", "n": 0})
    with pytest.raises(mushroomdb.TransactionConflict):
        with db.transaction() as tx:
            tx.execute("MATCH (n:P {id: 'acct'}) SET n.n = 1")
            db.upsert_node("P", "acct", {"n": 9})
    assert db.query("MATCH (n:P) RETURN n.n AS n") == [{"n": 9}]


# -- which handles may begin -------------------------------------------------


def test_a_read_only_handle_refuses_to_begin(tmp_path, db):
    db.insert_node("P", "seed", {"id": "seed"})
    reader = _reader(str(tmp_path / "store"))
    try:
        with pytest.raises(mushroomdb.ReadOnly):
            reader.begin_transaction()
        with pytest.raises(mushroomdb.ReadOnly):
            reader.transaction()
    finally:
        reader.close()


def test_a_scoped_handles_transaction_reads_through_the_scope_and_never_writes(db):
    db.insert_node("P", "mine", {"id": "mine"}, namespace="a")
    db.insert_node("P", "theirs", {"id": "theirs"}, namespace="b")
    scoped = db.scoped(namespace="a")
    with scoped.transaction() as tx:
        assert tx.execute("MATCH (n:P) RETURN n.id AS id") == [{"id": "mine"}]
        with pytest.raises(mushroomdb.ReadOnly):
            tx.execute("CREATE (n:P {id: 'x'})")
        # The refused write did not end the transaction.
        assert tx.execute("MATCH (n:P) RETURN n.id AS id") == [{"id": "mine"}]
    assert "x" not in _ids(db)
    tx = scoped.begin_transaction()
    assert isinstance(tx.commit(), int)


def test_a_scoped_transaction_reads_the_snapshot_it_began_on(db):
    db.insert_node("P", "a1", {"id": "a1"}, namespace="a")
    scoped = db.scoped(namespace="a")
    tx = scoped.begin_transaction()
    db.insert_node("P", "a2", {"id": "a2"}, namespace="a")
    assert tx.execute("MATCH (n:P) RETURN n.id AS id") == [{"id": "a1"}]
    tx.rollback()


# -- close, lock, and the handle after the wrap ------------------------------


def test_close_with_an_open_transaction_fails_clearly(db):
    tx = db.begin_transaction()
    with pytest.raises(RuntimeError, match="open transaction"):
        db.close()
    # Still open and usable; finishing the transaction lets close succeed.
    tx.execute("CREATE (n:P {id: 'a'})")
    tx.commit()
    db.close()
    with pytest.raises(RuntimeError, match="closed"):
        db.begin_transaction()


def test_a_dropped_transaction_is_a_rollback_and_does_not_block_close(db):
    tx = db.begin_transaction()
    tx.execute("CREATE (n:P {id: 'a'})")
    del tx
    gc.collect()
    assert _ids(db) == []
    db.close()


def test_the_write_lock_is_still_held_after_a_transaction(tmp_path, db):
    path = str(tmp_path / "store")
    with db.transaction() as tx:
        tx.execute("CREATE (n:P {id: 'a'})")
    with pytest.raises(mushroomdb.MushroomBusy):
        GraphDb.open(path)
    db.close()
    reopened = GraphDb.open(path)
    try:
        assert _ids(reopened) == ["a"]
    finally:
        reopened.close()


def test_the_lock_outlives_close_until_no_transaction_holds_the_store(tmp_path, db):
    path = str(tmp_path / "store")
    tx = db.begin_transaction()
    with pytest.raises(RuntimeError):
        db.close()
    with pytest.raises(mushroomdb.MushroomBusy):
        GraphDb.open(path)
    tx.rollback()


def test_the_handle_keeps_working_after_it_has_begun_a_transaction(db):
    db.insert_node("P", "before", {"id": "before"})
    db.transaction().__enter__().rollback()
    db.insert_node("P", "after", {"id": "after"})
    assert db.upsert_node("P", "after", {"id": "after", "v": 1}) == "updated"
    db.query_write("CREATE (n:P {id: 'cypher'})")
    assert db.refresh() == 0
    with db.transaction() as tx:
        rows = tx.execute("MATCH (n:P) RETURN n.id AS id")
        assert sorted(r["id"] for r in rows) == ["after", "before", "cypher"]
    assert db.stats()["nodes_live"] == 3


def test_a_store_written_through_transactions_reopens_intact(tmp_path):
    path = str(tmp_path / "store")
    db = GraphDb.open(path)
    for i in range(5):
        with db.transaction() as tx:
            tx.execute("CREATE (n:P {id: $id})", {"id": f"k{i}"})
    db.close()
    reopened = GraphDb.open(path)
    try:
        assert _ids(reopened) == [f"k{i}" for i in range(5)]
    finally:
        reopened.close()


# -- threads -----------------------------------------------------------------


def _join(threads):
    for t in threads:
        t.join(_JOIN_SECONDS)
        assert not t.is_alive(), "a thread did not finish: deadlock"


def test_many_concurrent_transactions_from_threads(db):
    errors: list[BaseException] = []

    def work(i: int) -> None:
        try:
            for j in range(5):
                with db.transaction() as tx:
                    tx.execute("CREATE (n:P {id: $id})", {"id": f"t{i}-{j}"})
        except BaseException as exc:  # noqa: BLE001 - reported below
            errors.append(exc)

    threads = [threading.Thread(target=work, args=(i,), daemon=True) for i in range(8)]
    for t in threads:
        t.start()
    _join(threads)
    assert errors == []
    assert len(_ids(db)) == 40


def test_one_transaction_used_from_two_threads_is_serialised(db):
    tx = db.begin_transaction()
    errors: list[BaseException] = []

    def work(i: int) -> None:
        try:
            for j in range(10):
                tx.execute("CREATE (n:P {id: $id})", {"id": f"t{i}-{j}"})
        except BaseException as exc:  # noqa: BLE001 - reported below
            errors.append(exc)

    threads = [threading.Thread(target=work, args=(i,), daemon=True) for i in range(2)]
    for t in threads:
        t.start()
    _join(threads)
    assert errors == []
    tx.commit()
    assert len(_ids(db)) == 20


def test_a_handle_write_does_not_wait_on_an_idle_open_transaction(db):
    tx = db.begin_transaction()
    done = threading.Event()

    def write() -> None:
        db.insert_node("P", "w", {"id": "w"})
        done.set()

    t = threading.Thread(target=write, daemon=True)
    t.start()
    assert done.wait(_JOIN_SECONDS)
    tx.rollback()


# -- the rest of the handle after the wrap -----------------------------------


def test_rules_fire_on_a_transactions_commit(db):
    db.create_rule(
        {
            "name": "same_team",
            "src_label": "P",
            "dst_label": "P",
            "predicate": {"FieldEqual": {"field": "team"}},
            "edge_type": "SAME_TEAM",
            "weight_prop": None,
        }
    )
    with db.transaction() as tx:
        tx.execute("CREATE (n:P {id: 'a', team: 'red'})")
        tx.execute("CREATE (n:P {id: 'b', team: 'red'})")
    rows = db.query("MATCH (a:P)-[r:SAME_TEAM]->(b:P) RETURN count(r) AS c")
    assert rows[0]["c"] >= 1


def test_memory_and_scoped_reads_work_after_the_wrap(db):
    db.begin_transaction().rollback()
    db.memory.remember("Ada likes graphs", about=["ada"])
    assert db.scoped(namespace="default").query("MATCH (n) RETURN count(n) AS c")
    assert db.memory.recall("graphs")


def test_the_lock_survives_plain_writes_on_a_wrapped_handle(tmp_path, db):
    path = str(tmp_path / "store")
    db.begin_transaction().rollback()
    db.insert_node("P", "a", {"id": "a"})
    db.query_write("CREATE (n:P {id: 'b'})")
    db.set_prop("a", "v", 1)
    with pytest.raises(mushroomdb.MushroomBusy):
        GraphDb.open(path)


def test_a_transaction_and_handle_writes_interleave_from_two_threads(db):
    tx = db.begin_transaction()
    errors: list[BaseException] = []

    def tx_side() -> None:
        try:
            for j in range(20):
                tx.execute("CREATE (n:P {id: $id})", {"id": f"tx{j}"})
        except BaseException as exc:  # noqa: BLE001 - reported below
            errors.append(exc)

    def handle_side() -> None:
        try:
            for j in range(20):
                db.insert_node("P", f"h{j}", {"id": f"h{j}"})
        except BaseException as exc:  # noqa: BLE001 - reported below
            errors.append(exc)

    threads = [threading.Thread(target=f, daemon=True) for f in (tx_side, handle_side)]
    for t in threads:
        t.start()
    _join(threads)
    assert errors == []
    tx.commit()
    assert len(_ids(db)) == 40


def test_the_gil_is_released_while_a_statement_runs(db):
    """A long `execute` must not stall other Python threads."""
    import time

    for i in range(1500):
        db.insert_node("P", f"k{i}", {"id": f"k{i}", "g": i % 7})
    tx = db.begin_transaction()
    ticks = 0
    stop = threading.Event()

    def spin() -> None:
        nonlocal ticks
        while not stop.is_set():
            ticks += 1
            time.sleep(0)

    t = threading.Thread(target=spin, daemon=True)
    t.start()
    time.sleep(0.05)
    before = ticks
    started = time.perf_counter()
    rows = tx.execute("MATCH (a:P), (b:P) RETURN count(*) AS c")
    elapsed = time.perf_counter() - started
    during = ticks - before
    stop.set()
    t.join(_JOIN_SECONDS)
    assert rows == [{"c": 1500 * 1500}]
    assert elapsed > 0.05, "the statement was too quick to say anything"
    # A thread that held the GIL for the whole statement would tick ~0 times.
    assert during > 20, f"{during} ticks in {elapsed:.2f}s"
    tx.rollback()
