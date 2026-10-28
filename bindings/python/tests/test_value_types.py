"""The five format-V11 value types in Python: Date, LocalDateTime, DateTime,
Duration and Binary.

Every conversion is tested end to end: the value is written to a store
(`insert_node`) and read back (`node_info`), which is the Python object ->
engine `Value` -> WAL -> engine `Value` -> Python object path a real caller
takes. The first such write moves the store to the V11 snapshot format.
"""

from __future__ import annotations

import datetime as dt
import itertools

import pytest

import mushroomdb
from mushroomdb import Duration, GraphDb

UTC = dt.timezone.utc


def tz(hours: int = 0, minutes: int = 0, seconds: int = 0) -> dt.timezone:
    return dt.timezone(dt.timedelta(hours=hours, minutes=minutes, seconds=seconds))


_DB: GraphDb | None = None
_KEYS = itertools.count()


@pytest.fixture(scope="module", autouse=True)
def _store(tmp_path_factory):
    global _DB
    _DB = GraphDb.open(str(tmp_path_factory.mktemp("value-types") / "db"))
    yield _DB
    _DB.close()
    _DB = None


def roundtrip(value):
    """Store `value` on a fresh node and read it back from the store."""
    assert _DB is not None
    key = f"n{next(_KEYS)}"
    _DB.insert_node("V", key, {"v": value})
    return _DB.node_info(key)["props"]["v"]


def snapshot_version(path) -> int | None:
    snap = path / "snapshot.bin"
    if not snap.exists():
        return None
    head = snap.read_bytes()[:6]
    assert head[:4] == b"GDB1"
    return int.from_bytes(head[4:6], "little")


# ---------------------------------------------------------------------------
# Round trips
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "value",
    [
        dt.date(2024, 1, 15),
        dt.date(1970, 1, 1),
        dt.date(1969, 12, 31),
        dt.date(1, 1, 1),
        dt.date(9999, 12, 31),
        dt.date(2024, 2, 29),
    ],
)
def test_date_roundtrip(value):
    out = roundtrip(value)
    assert out == value
    assert type(out) is dt.date


@pytest.mark.parametrize(
    "value",
    [
        dt.datetime(2024, 1, 15, 12, 34, 56, 789012),
        dt.datetime(1970, 1, 1),
        dt.datetime(1969, 12, 31, 23, 59, 59, 999999),
        dt.datetime(1, 1, 1),
        dt.datetime(9999, 12, 31, 23, 59, 59, 999999),
    ],
)
def test_naive_datetime_is_a_local_datetime(value):
    out = roundtrip(value)
    assert out == value
    assert out.tzinfo is None


@pytest.mark.parametrize(
    "value",
    [
        dt.datetime(2024, 1, 15, 12, 34, 56, 789012, tzinfo=UTC),
        dt.datetime(2024, 1, 15, 12, 34, 56, 789012, tzinfo=tz(5, 30)),
        dt.datetime(2024, 1, 15, 7, 0, tzinfo=tz(-5)),
        dt.datetime(2024, 1, 15, tzinfo=tz(18)),
        dt.datetime(2024, 1, 15, tzinfo=tz(-18)),
        dt.datetime(2024, 1, 15, tzinfo=tz(1, 2, 3)),
        dt.datetime(1969, 12, 31, 23, 59, 59, 1, tzinfo=tz(-0, -30)),
        dt.datetime(9999, 12, 31, 23, 59, 59, 999999, tzinfo=tz(0)),
        dt.datetime(1, 1, 1, tzinfo=tz(5)),
        dt.datetime(9999, 12, 31, 23, tzinfo=tz(-18)),
    ],
)
def test_aware_datetime_keeps_its_offset(value):
    out = roundtrip(value)
    assert out == value
    assert out.utcoffset() == value.utcoffset()
    assert (out.year, out.month, out.day, out.hour, out.minute) == (
        value.year,
        value.month,
        value.day,
        value.hour,
        value.minute,
    )


def test_same_instant_different_offset_both_keep_their_offset():
    a = dt.datetime(2024, 1, 15, 12, tzinfo=UTC)
    b = dt.datetime(2024, 1, 15, 7, tzinfo=tz(-5))
    assert a == b
    assert roundtrip(a).utcoffset() != roundtrip(b).utcoffset()


def test_datetime_is_not_mistaken_for_a_date():
    assert isinstance(roundtrip(dt.datetime(2024, 1, 15)), dt.datetime)
    assert type(roundtrip(dt.date(2024, 1, 15))) is dt.date


def test_tzinfo_whose_utcoffset_is_none_is_naive():
    class Unknown(dt.tzinfo):
        def utcoffset(self, when):
            return None

        def dst(self, when):
            return None

    out = roundtrip(dt.datetime(2024, 1, 15, 1, 2, 3, tzinfo=Unknown()))
    assert out == dt.datetime(2024, 1, 15, 1, 2, 3)
    assert out.tzinfo is None


def test_duration_type():
    dur = Duration(months=1, days=2, nanos=3)
    assert dur.months == 1 and dur.days == 2 and dur.nanos == 3
    assert dur == Duration(months=1, days=2, nanos=3)
    assert dur != Duration(months=1, days=2, nanos=4)
    assert hash(dur) == hash(Duration(1, 2, 3))
    assert repr(dur) == "Duration(months=1, days=2, nanos=3)"
    assert Duration() == Duration(0, 0, 0)
    with pytest.raises(AttributeError):
        dur.months = 5  # read-only


@pytest.mark.parametrize(
    "dur",
    [
        Duration(),
        Duration(1, 2, 3),
        Duration(-1, -2, -3),
        Duration(2**31 - 1, 2**31 - 1, 2**63 - 1),
        Duration(-(2**31), -(2**31), -(2**63)),
    ],
)
def test_duration_roundtrip(dur):
    assert roundtrip(dur) == dur


def test_a_month_is_not_thirty_days():
    assert Duration(months=1) != Duration(days=30)
    assert roundtrip(Duration(months=1)) == Duration(months=1, days=0, nanos=0)


@pytest.mark.parametrize("field", ["months", "days"])
def test_duration_i32_fields_refuse_overflow(field):
    with pytest.raises(OverflowError):
        Duration(**{field: 2**31})
    with pytest.raises(OverflowError):
        Duration(nanos=2**63)


@pytest.mark.parametrize(
    "value",
    [b"", b"\x00\xff\xde\xad", bytes(range(256)), b"a" * 100_000],
)
def test_binary_roundtrip(value):
    out = roundtrip(value)
    assert out == value
    assert type(out) is bytes


def test_bytearray_and_memoryview_become_binary():
    raw = b"\x01\x02\x03\x04"
    assert roundtrip(bytearray(raw)) == raw
    assert roundtrip(memoryview(raw)) == raw
    # A strided view is copied in logical order.
    assert roundtrip(memoryview(raw)[::2]) == b"\x01\x03"
    assert type(roundtrip(bytearray(raw))) is bytes


def test_new_types_nest_in_lists_and_maps():
    value = {
        "when": [dt.date(2024, 1, 15), dt.datetime(2024, 1, 15, tzinfo=tz(2))],
        "blob": b"\x00",
        "span": Duration(1, 0, 0),
    }
    assert roundtrip(value) == value


def test_existing_types_are_unchanged():
    for value in (1, 1.5, "x", True, [1, "a"], {"k": 2}):
        assert roundtrip(value) == value
    # bool is still a bool, not an int.
    assert roundtrip(True) is True


# ---------------------------------------------------------------------------
# Agreement with the wire text: a value written from its ISO 8601 text by
# Cypher reads back in Python as the object that text names
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "cypher, expected",
    [
        ("date('2024-01-15')", dt.date(2024, 1, 15)),
        ("localdatetime('2024-01-15T12:34:56.789012')", dt.datetime(2024, 1, 15, 12, 34, 56, 789012)),
        (
            "datetime('2024-01-15T12:34:56.789012+05:30')",
            dt.datetime(2024, 1, 15, 12, 34, 56, 789012, tzinfo=tz(5, 30)),
        ),
        ("duration('P1Y2M3DT4H5M6.789S')", Duration(months=14, days=3, nanos=14_706_789_000_000)),
    ],
)
def test_iso_text_written_by_cypher_reads_back_as_the_python_object(cypher, expected):
    assert _DB is not None
    key = f"n{next(_KEYS)}"
    _DB.insert_node("V", key, {})
    _DB.query_write(f"MATCH (n:V) WHERE n.id = '{key}' SET n.v = {cypher}")
    out = _DB.node_info(key)["props"]["v"]
    assert out == expected
    if isinstance(expected, dt.datetime):
        assert out.utcoffset() == expected.utcoffset()


# ---------------------------------------------------------------------------
# What cannot be represented exactly is refused, not rounded
# ---------------------------------------------------------------------------


def test_offset_with_a_fractional_second_is_refused():
    odd = dt.timezone(dt.timedelta(hours=1, microseconds=1))
    with pytest.raises(ValueError, match="fractional second"):
        roundtrip(dt.datetime(2024, 1, 15, tzinfo=odd))


@pytest.mark.parametrize("seconds", [18 * 3600 + 1, -(18 * 3600 + 1), 23 * 3600])
def test_offset_beyond_eighteen_hours_is_refused(seconds):
    wide = dt.timezone(dt.timedelta(seconds=seconds))
    with pytest.raises(ValueError, match="18:00"):
        roundtrip(dt.datetime(2024, 1, 15, tzinfo=wide))


def test_timedelta_is_refused_and_points_at_duration():
    with pytest.raises(TypeError, match="mushroomdb.Duration"):
        roundtrip(dt.timedelta(days=1))


def test_time_of_day_is_still_not_a_value():
    with pytest.raises(TypeError, match="cannot convert"):
        roundtrip(dt.time(12, 0))


def test_nonzero_nanoseconds_are_refused_not_rounded():
    # A `pandas.Timestamp` is a `datetime` with a `nanosecond` the datetime
    # fields leave out; this subclass stands in for it without the dependency.
    class Stamp(dt.datetime):
        nanosecond = 0

    class FineStamp(dt.datetime):
        nanosecond = 1

    assert roundtrip(Stamp(2024, 1, 15, 1, 2, 3, 4)) == dt.datetime(2024, 1, 15, 1, 2, 3, 4)
    with pytest.raises(ValueError, match="nanosecond"):
        roundtrip(FineStamp(2024, 1, 15, 1, 2, 3, 4))
    with pytest.raises(ValueError, match="nanosecond"):
        roundtrip(FineStamp(2024, 1, 15, tzinfo=UTC))


# ---------------------------------------------------------------------------
# Writes move the store to the V11 format, and survive a reopen
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "value",
    [
        dt.date(2024, 1, 15),
        dt.datetime(2024, 1, 15, 1, 2, 3),
        dt.datetime(2024, 1, 15, 1, 2, 3, tzinfo=tz(-5)),
        Duration(months=1),
        b"\x00\x01",
    ],
)
def test_writing_a_new_type_moves_the_store_to_v11_and_survives_a_reopen(tmp_path, value):
    path = tmp_path / "db"
    db = GraphDb.open(str(path))
    db.insert_node("Event", "e0", {"n": 1})
    assert snapshot_version(path) is None
    db.insert_node("Event", "e1", {"on": value})
    assert snapshot_version(path) == 11
    assert db.node_info("e1")["props"]["on"] == value
    db.close()
    reopened = GraphDb.open(str(path))
    out = reopened.node_info("e1")["props"]["on"]
    assert out == value
    assert type(out) is type(value)
    rows = reopened.query("MATCH (n:Event) WHERE n.id = 'e1' RETURN n.on AS on")
    assert rows == [{"on": value}]
    reopened.snapshot()
    assert snapshot_version(path) == 11
    reopened.close()


def test_a_store_without_new_types_stays_v9(tmp_path):
    path = tmp_path / "db"
    db = GraphDb.open(str(path))
    db.insert_node("Event", "e1", {"n": 1, "s": "x"})
    db.snapshot()
    assert snapshot_version(path) == 9
    db.close()
