"""Ledger row 65: Python write methods release the GIL.

Before the fix every write held the GIL across the WAL fsync, which also made
`upsert_node`'s two lock acquisitions (read, then write) safe by accident. With
the GIL released, `upsert_node` has to be atomic in the engine call itself.

A GIL deadlock would hang the whole interpreter, so the threaded tests join
with a timeout and a hung worker fails the test rather than the run.
"""

from __future__ import annotations

import signal
import threading
import time

import pytest

import mushroomdb

_JOIN_SECONDS = 120


def _run_threads(targets):
    errors: list[BaseException] = []

    def wrap(fn):
        def run():
            try:
                fn()
            except BaseException as exc:  # noqa: BLE001 - reported below
                errors.append(exc)

        return run

    threads = [threading.Thread(target=wrap(t), daemon=True) for t in targets]
    for t in threads:
        t.start()
    for t in threads:
        t.join(_JOIN_SECONDS)
        assert not t.is_alive(), "a writer thread did not finish: deadlock"
    assert errors == []


@pytest.fixture
def db(tmp_path):
    handle = mushroomdb.GraphDb.open(str(tmp_path / "store"))
    yield handle
    handle.close()


def test_two_threads_writing_lose_nothing(db):
    per_thread = 150
    for t in range(2):
        for i in range(per_thread):
            db.insert_node("N", f"t{t}-{i}", {"v": -1})

    def writer(t):
        def run():
            for i in range(per_thread):
                db.set_prop(f"t{t}-{i}", "v", i)
                db.insert_edge("NEXT", f"t{t}-{i}", f"t{t}-{(i + 1) % per_thread}")

        return run

    _run_threads([writer(0), writer(1)])

    for t in range(2):
        for i in range(per_thread):
            info = db.node_info(f"t{t}-{i}")
            assert info["props"]["v"] == i


def test_writers_do_not_starve_other_python_threads(db):
    """A pure-Python thread keeps running while another thread writes."""
    db.insert_node("N", "a", {"v": 0})
    stop = threading.Event()
    ticks = [0]

    def ticker():
        while not stop.is_set():
            ticks[0] += 1
            time.sleep(0)

    def writer():
        for i in range(200):
            db.set_prop("a", "v", i)
        stop.set()

    _run_threads([ticker, writer])
    assert ticks[0] > 0
    assert db.node_info("a")["props"]["v"] == 199


def test_upsert_node_is_atomic_under_contention(db):
    """Row 65's precondition: read-then-write is one engine call.

    Four threads upsert the same keys. If the existence read and the write
    were separate lock acquisitions, two threads would both see the key absent
    and the loser's insert would raise `DuplicateKey`.
    """
    keys = [f"k{i}" for i in range(60)]
    outcomes: list[str] = []
    guard = threading.Lock()

    def worker(tid):
        def run():
            for key in keys:
                result = db.upsert_node("N", key, {f"from{tid}": tid})
                with guard:
                    outcomes.append(result)

        return run

    _run_threads([worker(t) for t in range(4)])

    assert outcomes.count("inserted") == len(keys)  # exactly one inserter per key
    assert outcomes.count("updated") == len(keys) * 3
    for key in keys:
        props = db.node_info(key)["props"]
        assert {f"from{t}": t for t in range(4)} == {
            k: v for k, v in props.items() if k.startswith("from")
        }


def test_upsert_node_wrong_label_still_raises_value_error(db):
    db.upsert_node("A", "x", {"p": 1})
    with pytest.raises(ValueError, match="already exists with label 'A', not 'B'"):
        db.upsert_node("B", "x", {"p": 1})


def test_python_exception_during_write_does_not_poison_the_handle(db):
    db.insert_node("N", "a", {"v": 0})
    with pytest.raises(TypeError):
        db.set_prop("a", "v", object())  # conversion fails before the lock
    with pytest.raises(mushroomdb.KeyNotFound):
        db.set_prop("missing", "v", 1)  # engine error inside the lock
    db.set_prop("a", "v", 7)
    assert db.node_info("a")["props"]["v"] == 7


def test_keyboard_interrupt_during_writes_does_not_poison_the_handle(db):
    """A signal handler raising in the main thread cannot corrupt the lock.

    The GIL is released during a write, so the interrupt is delivered between
    calls, never inside the store critical section.
    """
    db.insert_node("N", "a", {"v": 0})

    def interrupt(_signum, _frame):
        raise KeyboardInterrupt

    previous = signal.signal(signal.SIGALRM, interrupt)
    done = 0
    try:
        signal.setitimer(signal.ITIMER_REAL, 0.02)
        try:
            for i in range(1_000_000):
                db.set_prop("a", "v", i)
                done = i
        except KeyboardInterrupt:
            pass
        else:
            pytest.fail("the interrupt never arrived")
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)

    assert db.node_info("a")["props"]["v"] in (done, done + 1)
    db.set_prop("a", "v", -5)
    assert db.node_info("a")["props"]["v"] == -5
