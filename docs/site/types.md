# Property value types

Properties can hold integers, floats, strings, booleans, lists and maps, plus
five types introduced in 0.10: `Date`, `LocalDateTime`, `DateTime`, `Duration`
and `Binary`. A plain date-looking string stays a string; use a typed value
or the tagged JSON form when its type matters.

The first write that stores one of these five types, including inside a list
or map, opts the store into **V11** before the value reaches the WAL. Opening
an older store alone does not opt in. V11 is sticky and cannot be read by
0.9.x, even after the typed value is removed. A store that uses no V11 feature
stays V9/V10-readable. See [Format stability](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/format-stability.md#v11-is-opt-in-and-sticky).

## Representations

| Type | Rust storage | JSON value | Python value | TypeScript wire type |
|---|---|---|---|---|
| Date | `Date(i32)`: days since 1970-01-01 | `{"$date":"2024-01-15"}` | `datetime.date` | `DateValue` |
| Local date-time | `LocalDateTime(i64)`: microseconds since 1970-01-01T00:00:00, without a zone | `{"$ldt":"2024-01-15T12:34:56.789012"}` | naive `datetime.datetime` | `LocalDateTimeValue` |
| Date-time with offset | `DateTime { micros_utc: i64, offset_seconds: i32 }` | `{"$dt":"2024-01-15T12:34:56.789012+05:30"}` | aware `datetime.datetime` | `DateTimeValue` |
| Duration | `Duration { months: i32, days: i32, nanos: i64 }` | `{"$dur":"P1M3DT4S"}` | `mushroomdb.Duration` | `DurationValue` |
| Binary | `Binary(Vec<u8>)` | `{"$binary":"AP8="}` | `bytes` | `BinaryValue` |

Date-time offsets are whole seconds within ±18 hours. The original offset is
retained; the instant is stored as UTC microseconds. Local date-times have no
zone and are not implicitly interpreted in the machine's local zone.

A duration keeps its three components apart: one month is not 30 days, and
one day is not automatically converted to a fixed number of nanoseconds.
`P1M3DT4S` has months=1, days=3 and nanos=4,000,000,000.

Binary is an uninterpreted byte sequence, at most **1,048,576 decoded bytes
(one MiB)** per blob. JSON carries standard base64 with padding; `AP8=` is
the two bytes `0x00, 0xff`, not a UTF-8 string.

The tags are one-key JSON objects. Lists and maps retain typed values at any
depth. A query can return null, including inside a list or map, but null is
not stored, including inside a property list or map. Removing a property makes it absent; see
[SET and REMOVE](query.md#write-statements).

## Cypher and the CLI

Temporal constructors create typed query values. They opt into V11 only when
a write stores the result.

```sh
mushroomdb query type-demo "RETURN date('2024-01-15') AS day, localdatetime('2024-01-15T12:34:56.789012') AS local, datetime('2024-01-15T12:34:56.789012+05:30') AS instant, duration('P1M3DT4S') AS span"
```

```text
columns: day, local, instant, span
  day=2024-01-15  local=2024-01-15T12:34:56.789012  instant=2024-01-15T12:34:56.789012+05:30  span=P1M3DT4S
```

Store a date on an existing or new node:

```sh
mushroomdb query type-demo "CREATE (:Event {id:'launch', day:date('2024-01-15')})"
mushroomdb query type-demo "MATCH (e:Event) RETURN e.day AS day"
```

```text
columns: day
  day=2024-01-15
```

The [query reference](query.md#temporal-values) covers constructors,
arithmetic and supported functions. `time()` and `localtime()` are outside
0.10; there is no stored Time or LocalTime type.

## Python

Use native date/time/byte values and the public `Duration` class:

```python
from datetime import date, datetime, timedelta, timezone
import mushroomdb

db = mushroomdb.GraphDb.open("typed-python-demo")
db.insert_node("Event", "launch", {
    "day": date(2024, 1, 15),
    "local": datetime(2024, 1, 15, 12, 34, 56, 789012),
    "instant": datetime(2024, 1, 15, 12, 34, 56, 789012,
                        tzinfo=timezone(timedelta(hours=5, minutes=30))),
    "span": mushroomdb.Duration(months=1, days=3, nanos=4_000_000_000),
    "payload": b"\x00\xff",
})
db.close()
```

`Duration` is immutable and hashable, with `months`, `days` and `nanos`
attributes. A `timedelta` used as a
property is refused because it cannot represent calendar months. The
`timedelta` above only constructs the time-zone offset.

Python values must be exact: an offset containing fractional seconds or a
timestamp with nonzero sub-microsecond precision is refused, rather than
rounded. Reading a date outside Python's years 1–9999 is also refused. See
the [Python API](python-api.md) for accepted inputs and errors.

## TypeScript and JSON

The client keeps the tagged wire forms; it does not turn microsecond values
into JavaScript `Date` objects, which have millisecond precision.

```typescript
import { asDate, asDateTime, asDuration, asBinary, binaryBytes } from "mushroomdb-client";
import type { DateValue, LocalDateTimeValue, DateTimeValue, DurationValue, BinaryValue } from "mushroomdb-client";

const day: DateValue = { $date: "2024-01-15" };
const local: LocalDateTimeValue = { $ldt: "2024-01-15T12:34:56.789012" };
const instant: DateTimeValue = { $dt: "2024-01-15T12:34:56.789012+05:30" };
const span: DurationValue = { $dur: "P1M3DT4S" };
const payload: BinaryValue = { $binary: "AP8=" };

asDate(day);          // "2024-01-15"
asDateTime(instant);  // "2024-01-15T12:34:56.789012+05:30"
asDuration(span);     // "P1M3DT4S"
asBinary(payload);   // "AP8=" (base64 text)
binaryBytes(payload); // Uint8Array containing 0, 255
```

`asLocalDateTime` returns the local date-time text. Every `as…` helper returns
`undefined` when its argument has a different shape. See the
[TypeScript client](typescript.md) and [HTTP reference](http.md).

## Equality, ordering and indexes

- Dates and local date-times compare chronologically within their own type.
- Query equality for date-times compares UTC instants, so two offsets can
  describe equal values. Sorting compares the instant first and may use the
  stored offset to break a tie.
- Durations support component-wise equality. Inequality predicates do not
  give them a chronological order; one month cannot be ranked against 31 days.
- Binary values compare lexicographically by bytes, without coercing strings.
- Cross-temporal types are not implicitly coerced: a Date is not a local
  date-time at midnight. Null comparisons use query null semantics.

Date, local date-time, date-time and Binary have equality-index keys. Temporal
values can also answer supported [range-index predicates](indexes.md#range-predicates-and-ordering).
Duration, lists and maps have no index key. Binary equality is supported;
Binary range-query acceleration is outside the advertised guarantee.

## Arrow boundaries

The HTTP query Arrow bridge represents date-times as UTC timestamps, losing
each value's original offset. A Duration column with any months falls back to
ISO text; without months, it uses microseconds, truncating sub-microsecond
nanoseconds and saturating at the signed 64-bit limits.

Python table export is a different surface: node/edge `properties` carry
tagged JSON and retain the forms above. See [Arrow, pandas and polars](arrow.md)
for that export contract.
