"""The five format-V11 value types in Python: Date, LocalDateTime, DateTime,
Duration and Binary.

Until TY-12 lets the engine store them, a write carrying one is refused. So the
conversions are tested through `_value_roundtrip` (Python object -> engine
`Value` -> Python object, the two halves a write and a read use) and the
refusal is tested through a real write. TY-12 re-tests both end to end.
"""

from __future__ import annotations

import datetime as dt

import pytest

import mushroomdb
from mushroomdb import Duration, GraphDb, _value_roundtrip, _value_tagged_text

UTC = dt.timezone.utc


def tz(hours: int = 0, minutes: int = 0, seconds: int = 0) -> dt.timezone:
    return dt.timezone(dt.timedelta(hours=hours, minutes=minutes, seconds=seconds))


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
    out = _value_roundtrip(value)
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
    out = _value_roundtrip(value)
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
    out = _value_roundtrip(value)
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
    assert _value_roundtrip(a).utcoffset() != _value_roundtrip(b).utcoffset()


def test_datetime_is_not_mistaken_for_a_date():
    assert isinstance(_value_roundtrip(dt.datetime(2024, 1, 15)), dt.datetime)
    assert type(_value_roundtrip(dt.date(2024, 1, 15))) is dt.date


def test_tzinfo_whose_utcoffset_is_none_is_naive():
    class Unknown(dt.tzinfo):
        def utcoffset(self, when):
            return None

        def dst(self, when):
            return None

    out = _value_roundtrip(dt.datetime(2024, 1, 15, 1, 2, 3, tzinfo=Unknown()))
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
    assert _value_roundtrip(dur) == dur


def test_a_month_is_not_thirty_days():
    assert Duration(months=1) != Duration(days=30)
    assert _value_roundtrip(Duration(months=1)) == Duration(months=1, days=0, nanos=0)


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
    out = _value_roundtrip(value)
    assert out == value
    assert type(out) is bytes


def test_bytearray_and_memoryview_become_binary():
    raw = b"\x01\x02\x03\x04"
    assert _value_roundtrip(bytearray(raw)) == raw
    assert _value_roundtrip(memoryview(raw)) == raw
    # A strided view is copied in logical order.
    assert _value_roundtrip(memoryview(raw)[::2]) == b"\x01\x03"
    assert type(_value_roundtrip(bytearray(raw))) is bytes


def test_new_types_nest_in_lists_and_maps():
    value = {
        "when": [dt.date(2024, 1, 15), dt.datetime(2024, 1, 15, tzinfo=tz(2))],
        "blob": b"\x00",
        "span": Duration(1, 0, 0),
    }
    assert _value_roundtrip(value) == value


def test_existing_types_are_unchanged():
    for value in (1, 1.5, "x", True, [1, "a"], {"k": 2}):
        assert _value_roundtrip(value) == value
    # bool is still a bool, not an int.
    assert _value_roundtrip(True) is True


# ---------------------------------------------------------------------------
# Agreement with the wire text (what the server's JSON tags carry)
# ---------------------------------------------------------------------------


def test_tagged_text_matches_the_server_json_forms():
    assert _value_tagged_text(dt.date(2024, 1, 15)) == ("$date", "2024-01-15")
    assert _value_tagged_text(dt.datetime(2024, 1, 15, 12, 34, 56, 789012)) == (
        "$ldt",
        "2024-01-15T12:34:56.789012",
    )
    assert _value_tagged_text(
        dt.datetime(2024, 1, 15, 12, 34, 56, 789012, tzinfo=tz(5, 30))
    ) == ("$dt", "2024-01-15T12:34:56.789012+05:30")
    assert _value_tagged_text(Duration(months=14, days=3, nanos=14_706_789_000_000)) == (
        "$dur",
        "P1Y2M3DT4H5M6.789S",
    )
    assert _value_tagged_text(b"\xde\xad\xbe\xef") == ("$binary", "3q2+7w==")
    assert _value_tagged_text("text") is None


# ---------------------------------------------------------------------------
# What cannot be represented exactly is refused, not rounded
# ---------------------------------------------------------------------------


def test_offset_with_a_fractional_second_is_refused():
    odd = dt.timezone(dt.timedelta(hours=1, microseconds=1))
    with pytest.raises(ValueError, match="fractional second"):
        _value_roundtrip(dt.datetime(2024, 1, 15, tzinfo=odd))


@pytest.mark.parametrize("seconds", [18 * 3600 + 1, -(18 * 3600 + 1), 23 * 3600])
def test_offset_beyond_eighteen_hours_is_refused(seconds):
    wide = dt.timezone(dt.timedelta(seconds=seconds))
    with pytest.raises(ValueError, match="18:00"):
        _value_roundtrip(dt.datetime(2024, 1, 15, tzinfo=wide))


def test_timedelta_is_refused_and_points_at_duration():
    with pytest.raises(TypeError, match="mushroomdb.Duration"):
        _value_roundtrip(dt.timedelta(days=1))


def test_time_of_day_is_still_not_a_value():
    with pytest.raises(TypeError, match="cannot convert"):
        _value_roundtrip(dt.time(12, 0))


# ---------------------------------------------------------------------------
# Writes, until TY-12
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "value, type_name",
    [
        (dt.date(2024, 1, 15), "Date"),
        (dt.datetime(2024, 1, 15, 1, 2, 3), "LocalDateTime"),
        (dt.datetime(2024, 1, 15, 1, 2, 3, tzinfo=UTC), "DateTime"),
        (Duration(months=1), "Duration"),
        (b"\x00\x01", "Binary"),
    ],
)
def test_writing_a_new_type_is_refused_until_the_engine_stores_it(tmp_path, value, type_name):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(mushroomdb.MushroomError, match=f"{type_name} values cannot be stored"):
        db.insert_node("Event", "e1", {"on": value})
    assert db.node_info("e1") is None
    db.close()
