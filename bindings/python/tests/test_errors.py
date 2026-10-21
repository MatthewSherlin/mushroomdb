"""Error classes — one stable Python class per engine error (v0.6.10 §5.7).

Every class descends from `MushroomError`, which descends from `RuntimeError`,
so the change is non-breaking: every `except RuntimeError` written against an
earlier release keeps catching exactly what it caught.

`.code` is the compatibility surface. Classes may be added; a code may never be
respelled.
"""

from __future__ import annotations

import pathlib
import re

import pytest

import mushroomdb
from mushroomdb import GraphDb

# ---------------------------------------------------------------------------
# The variant list, read out of the Rust source
# ---------------------------------------------------------------------------

_TYPES_RS = (
    pathlib.Path(__file__).resolve().parents[3] / "crates" / "core-storage" / "src" / "types.rs"
)


def _snake(name: str) -> str:
    """`CommitOutOfRange` -> `commit_out_of_range`; `Io` -> `io`."""
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])", "_", name).lower()


def _graph_error_variants() -> list[str]:
    """Parse `enum GraphError` out of `crates/core-storage/src/types.rs`.

    Deliberately reads the Rust source rather than a list kept here: add a
    variant in Rust without adding a class to the binding and this fails.
    """
    assert _TYPES_RS.is_file(), f"cannot find the engine's error enum at {_TYPES_RS}"
    lines = _TYPES_RS.read_text().splitlines()
    start = next(i for i, ln in enumerate(lines) if ln.startswith("pub enum GraphError {"))
    end = next(i for i, ln in enumerate(lines[start + 1 :], start + 1) if ln == "}")
    # A variant sits at exactly four spaces of indent and starts uppercase;
    # its fields are indented deeper and start lowercase, and doc comments
    # start with `///`.
    variants = [
        m.group(1)
        for ln in lines[start + 1 : end]
        if (m := re.fullmatch(r"    ([A-Z]\w*)\s*[{(,].*", ln))
    ]
    assert len(variants) >= 18, f"parser found only {variants}"
    return variants


def _error_classes() -> dict[str, type]:
    """Every `MushroomError` subclass the module exports, keyed by `.code`."""
    out: dict[str, type] = {}
    for name, obj in vars(mushroomdb).items():
        if not isinstance(obj, type) or not issubclass(obj, mushroomdb.MushroomError):
            continue
        if obj is mushroomdb.MushroomError:
            continue
        code = getattr(obj, "code", None)
        assert isinstance(code, str), f"{name} carries no .code"
        assert code not in out, f"{name} and {out[code].__name__} share code {code!r}"
        out[code] = obj
    return out


def test_every_graph_error_variant_maps_to_a_distinct_class():
    variants = _graph_error_variants()
    want = {_snake(v) for v in variants}
    assert len(want) == len(variants), "two variants would share a code"
    assert set(_error_classes()) == want


# ---------------------------------------------------------------------------
# The compatibility guarantee
# ---------------------------------------------------------------------------


def test_the_base_is_a_runtime_error():
    assert issubclass(mushroomdb.MushroomError, RuntimeError)
    # The base names no variant, so it carries no code.
    assert mushroomdb.MushroomError.code is None
    for code, cls in _error_classes().items():
        assert issubclass(cls, mushroomdb.MushroomError), code
        assert issubclass(cls, RuntimeError), code


def test_every_class_still_catches_as_runtime_error(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(RuntimeError):  # the compatibility guarantee
        db.node_edges("nope")
    db.close()


def test_mushroom_busy_is_reparented_but_still_a_runtime_error():
    assert issubclass(mushroomdb.MushroomBusy, mushroomdb.MushroomError)
    assert issubclass(mushroomdb.MushroomBusy, RuntimeError)
    assert mushroomdb.MushroomBusy.code == "busy"


def test_busy_carries_its_holder(tmp_path):
    path = str(tmp_path / "db")
    db = GraphDb.open(path)
    with pytest.raises(mushroomdb.MushroomBusy) as e:
        GraphDb.open(path)
    assert e.value.code == "busy"
    assert e.value.holder is None or isinstance(e.value.holder, int)
    assert "store is busy" in str(e.value)
    db.close()


def test_code_reads_the_same_off_the_class_and_the_instance(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(mushroomdb.KeyNotFound) as e:
        db.node_edges("nope")
    assert e.value.code == mushroomdb.KeyNotFound.code == "key_not_found"
    db.close()


# ---------------------------------------------------------------------------
# One test per variant reachable from the binding
# ---------------------------------------------------------------------------


def test_key_not_found_is_a_class_with_the_key(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(mushroomdb.KeyNotFound) as e:
        db.node_edges("nope")
    assert e.value.code == "key_not_found"
    assert e.value.key == "nope"
    assert str(e.value) == "node key not found: nope"
    db.close()


def test_duplicate_key_carries_the_key(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {})
    with pytest.raises(mushroomdb.DuplicateKey) as e:
        db.insert_node("Person", "alice", {})
    assert e.value.code == "duplicate_key"
    assert e.value.key == "alice"
    assert str(e.value) == "duplicate node key: alice"
    db.close()


def test_commit_out_of_range_carries_its_range(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {})
    with pytest.raises(mushroomdb.CommitOutOfRange) as e:
        db.query_at(9999, "MATCH (n) RETURN key(n) AS k")
    assert e.value.code == "commit_out_of_range"
    assert e.value.commit == 9999
    assert isinstance(e.value.total, int) and isinstance(e.value.floor, int)
    assert e.value.floor == 0
    assert str(e.value).startswith("commit 9999 is out of range")
    db.close()


def test_query_error_keeps_its_detail_as_the_message(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"age": 30})
    with pytest.raises(mushroomdb.QueryError) as e:
        db.query("MATCH (n:Person) RETURN key(n.age) AS k")
    assert e.value.code == "query_error"
    # Verbatim: no `query error:` prefix was ever shown to a Python caller.
    assert str(e.value) == e.value.detail
    assert str(e.value) == "execute: key() argument must be a node variable (e.g. key(n))"
    db.close()


def test_rule_invalid_carries_its_detail(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    rule = {
        "name": "r",
        "src_label": "A",
        "dst_label": "B",
        "predicate": {"FieldEqual": {"field": "t"}},
        "edge_type": "E",
        "weight_prop": None,
    }
    assert db.create_rule(rule) is True
    with pytest.raises(mushroomdb.RuleInvalid) as e:
        db.create_rule(rule)
    assert e.value.code == "rule_invalid"
    assert e.value.detail == 'rule "r" already exists'
    assert str(e.value) == 'invalid rule: rule "r" already exists'
    db.close()


def test_namespace_immutable_carries_both_namespaces(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "a1", {}, namespace="tenant-a")
    with pytest.raises(mushroomdb.NamespaceImmutable) as e:
        db.set_prop("a1", "ns", "tenant-b")
    assert e.value.code == "namespace_immutable"
    assert e.value.key == "a1"
    assert e.value.from_ == "tenant-a"
    assert e.value.to == "tenant-b"
    assert "set at insert and cannot be changed" in str(e.value)
    db.close()


def test_cross_namespace_carries_both_ends(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "a1", {}, namespace="tenant-a")
    db.insert_node("Doc", "b1", {}, namespace="tenant-b")
    with pytest.raises(mushroomdb.CrossNamespace) as e:
        db.insert_edge("KNOWS", "a1", "b1")
    assert e.value.code == "cross_namespace"
    assert (e.value.src, e.value.src_ns) == ("a1", "tenant-a")
    assert (e.value.dst, e.value.dst_ns) == ("b1", "tenant-b")
    db.close()


def test_masked_read_only_on_a_scoped_write_query(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "a1", {})
    scoped = db.scoped(keys=["a1"])
    with pytest.raises(mushroomdb.MaskedReadOnly) as e:
        scoped.query("CREATE (n:Doc {id: 'z1'})")
    assert e.value.code == "masked_read_only"
    assert str(e.value) == "masked queries are read-only"
    assert db.node_info("z1") is None
    db.close()


def test_read_only_covers_both_handles_that_cannot_write(tmp_path):
    """The as-of refusal and the scoped-handle refusal are one class."""
    path = str(tmp_path / "db")
    db = GraphDb.open(path)
    db.insert_node("Doc", "a1", {})

    scoped = db.scoped(keys=["a1"])
    with pytest.raises(mushroomdb.ReadOnly) as scoped_err:
        scoped.insert_node("Doc", "z", {})
    assert scoped_err.value.code == "read_only"
    assert "this handle is scoped" in str(scoped_err.value)
    db.close()

    reader = GraphDb.open(path, read_only=True)
    with pytest.raises(mushroomdb.ReadOnly) as asof_err:
        reader.insert_node("Doc", "z", {})
    assert asof_err.value.code == "read_only"
    assert str(asof_err.value) == "as-of instances are read-only"
    reader.close()


def test_io_error_on_an_unopenable_path():
    with pytest.raises(mushroomdb.IoError) as e:
        GraphDb.open("/nonexistent-root-for-mushroomdb-tests/store")
    assert e.value.code == "io"
    assert str(e.value).startswith("io error: ")


def test_a_class_is_not_an_existence_oracle_for_a_scoped_reader(tmp_path):
    """A typed error must not tell a scoped caller more than the message did.

    Hidden and absent have to agree on class, `.code`, attributes and message —
    otherwise the new surface is a one-call existence probe that the old
    `RuntimeError` was not.
    """
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "visible", {})
    db.insert_node("Person", "hidden", {})
    scoped = db.scoped(keys=["visible"])

    with pytest.raises(RuntimeError) as hidden:
        scoped.node_edges("hidden")
    with pytest.raises(RuntimeError) as absent:
        scoped.node_edges("no-such-node")

    assert type(hidden.value) is type(absent.value)
    assert hidden.value.code == absent.value.code
    assert vars(hidden.value).keys() == vars(absent.value).keys()
    assert hidden.value.key == "hidden", "only the key the caller already named"
    assert str(hidden.value) == "node key not found: hidden"
    assert str(absent.value) == "node key not found: no-such-node"
    db.close()


# ---------------------------------------------------------------------------
# The stub declares what the module exports
# ---------------------------------------------------------------------------


def test_type_stub_declares_every_error_class():
    import importlib.util

    spec = importlib.util.find_spec("mushroomdb")
    assert spec is not None and spec.origin is not None
    stub = (pathlib.Path(spec.origin).parent / "__init__.pyi").read_text()
    assert "class MushroomError(RuntimeError):" in stub
    for cls in _error_classes().values():
        assert f"class {cls.__name__}(MushroomError):" in stub, cls.__name__


def test_constraint_and_size_errors_have_their_own_classes():
    # Raised by unique constraints and the value-size limit (0.10). The Python
    # API that declares a constraint arrives later; the classes and codes are
    # the compatibility surface now.
    for name, code in [
        ("ConstraintViolation", "constraint_violation"),
        ("ConstraintCreationConflict", "constraint_creation_conflict"),
        ("ValueTooLarge", "value_too_large"),
    ]:
        cls = getattr(mushroomdb, name)
        assert issubclass(cls, mushroomdb.MushroomError), name
        assert cls.code == code


def test_an_unwritable_directory_has_its_own_class_not_busy():
    # A permission refusal on the data directory is a deployment fault, not a
    # peer holding the lock (HO-12b). It must not be `MushroomBusy`.
    cls = mushroomdb.Unwritable
    assert issubclass(cls, mushroomdb.MushroomError)
    assert not issubclass(cls, mushroomdb.MushroomBusy)
    assert cls.code == "unwritable"
