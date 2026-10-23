"""The store lock is released before an engine error becomes a Python exception.

Defect #17. `graph_err` opens with `Python::attach`, so building the exception
needs the GIL. The four reads that call `with_scope` from inside
`py.allow_threads` — `find_similar`, `pairwise_similar`, `degree`, `degrees` —
take the store `Mutex` first and then the GIL; every other method holds the GIL
and then blocks on the same `Mutex`. Mapping the error while the guard is still
alive puts those two orders in a cycle on one `Arc<Inner>`.

**Why these run in a subprocess.** Once the cycle closes, the thread holding the
GIL is blocked on the `Mutex` forever, so nothing else in that interpreter runs
again — including the main thread, whose `Thread.join(timeout=...)` releases the
GIL to wait and can never reacquire it. An in-process threaded test would
therefore hang the whole run instead of failing it, which is worse than having
no test at all. A child process can be timed out and killed from outside, so the
failure is reported as a failure.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys
import textwrap

# Enough interleavings to close the cycle: on the unfixed binding the child
# hangs within the first few, long before it could finish.
_ITERATIONS = 2_000

# Generous: the fixed binding runs the whole workload in ~2s, and this machine
# runs several agents at once. Finite is the point — this is how long a real
# deadlock waits before it is reported as a failure.
_CHILD_TIMEOUT = 60.0

_CHILD = textwrap.dedent(
    """
    import sys
    import threading

    import mushroomdb
    from mushroomdb import GraphDb

    path, iterations = sys.argv[1], int(sys.argv[2])

    db = GraphDb.open(path)
    db.insert_node("Person", "a", {})

    stop = threading.Event()
    reader_failure = []

    def reader():
        # GIL -> Mutex: `node_info` holds the GIL for the whole call and
        # blocks on the store lock inside it.
        try:
            while not stop.is_set():
                db.node_info("a")
        except BaseException as exc:  # pragma: no cover - reported, not raised
            reader_failure.append(repr(exc))

    t = threading.Thread(target=reader)
    t.start()
    try:
        # Mutex -> GIL: `degree` releases the GIL, takes the store lock, and
        # fails with KeyNotFound while the guard is still alive.
        for _ in range(iterations):
            try:
                db.degree("no-such-key")
            except mushroomdb.KeyNotFound:
                pass
    finally:
        stop.set()
        t.join()

    if reader_failure:
        print("reader failed: " + reader_failure[0])
        sys.exit(2)
    print("ok")
    """
)


def _run_child(tmp_path) -> subprocess.CompletedProcess[str]:
    """Run the contending workload in a child, and kill it if it deadlocks.

    Returns the completed process. Raises nothing on a hang — the caller gets a
    `TimeoutExpired` to assert on, because a hang is the failure under test.
    """
    return subprocess.run(
        [sys.executable, "-c", _CHILD, str(tmp_path / "store"), str(_ITERATIONS)],
        capture_output=True,
        text=True,
        timeout=_CHILD_TIMEOUT,
    )


def test_error_raised_under_allow_threads_does_not_deadlock(tmp_path):
    """A read that errors inside `allow_threads` must not hold the lock into `Python::attach`."""
    try:
        done = _run_child(tmp_path)
    except subprocess.TimeoutExpired as hung:
        # `subprocess.run` has already SIGKILLed the child and reaped it, so
        # this test process is free to fail normally.
        raise AssertionError(
            f"deadlock: the contending workload did not finish within {_CHILD_TIMEOUT}s. "
            "A read that fails inside `py.allow_threads` took the GIL in `graph_err` "
            "while still holding the store Mutex, against another thread holding the "
            "GIL and waiting on that Mutex.\n"
            f"child stdout so far: {hung.stdout!r}\n"
            f"child stderr so far: {hung.stderr!r}"
        ) from None

    assert done.returncode == 0, (
        f"child exited {done.returncode}\nstdout: {done.stdout}\nstderr: {done.stderr}"
    )
    assert "ok" in done.stdout


# ---------------------------------------------------------------------------
# The rule, not just the one method that proved it
# ---------------------------------------------------------------------------

_LIB_RS = pathlib.Path(__file__).resolve().parents[1] / "src" / "lib.rs"

# Every helper that takes the store lock and then maps an engine error.
_LOCK_HELPERS = ("with_mut_unscoped", "with_ref", "with_scope")


def _method_body(source: str, name: str) -> str:
    """The text of `fn <name>` up to its closing brace at method indentation."""
    start = source.index(f"    fn {name}<")
    end = source.index("\n    }\n", start)
    return source[start:end]


def test_every_locking_helper_drops_the_guard_before_mapping_an_error():
    """`graph_err` takes the GIL, so no helper may call it holding the store lock.

    Only `with_scope` is reachable from inside `py.allow_threads` today, so only
    `with_scope` can be made to deadlock — which means a runtime test cannot
    defend the other two. It does not follow that they are free to break the
    rule: the moment any method wraps `with_ref` or `with_mut` in
    `allow_threads`, the same cycle is back, and nothing would have said so.
    This reads the source and holds all three to the one invariant.
    """
    source = _LIB_RS.read_text()
    for name in _LOCK_HELPERS:
        body = _method_body(source, name)
        assert "lock(&self.inner.0)" in body, f"{name}: fixture — this helper takes the lock"
        # One drop per store arm (plain, shared), so there may be several; the
        # error is mapped after the last of them.
        assert body.count("drop(guard)") >= 1, f"{name}: expected an explicit drop"
        drop_at = body.rindex("drop(guard)")
        maps = [
            i
            for i in range(len(body))
            if body.startswith("map_err(graph_err)", i)
        ]
        assert maps, f"{name}: fixture — this helper maps engine errors"
        assert all(i > drop_at for i in maps), (
            f"{name} maps a GraphError onto a Python class while the store guard is "
            "still alive. `graph_err` calls `Python::attach`, so that takes "
            "Mutex -> GIL; every other #[pymethods] fn takes GIL -> Mutex. Bind the "
            "result, `drop(guard)`, then map."
        )
