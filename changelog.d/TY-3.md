### Added
- **Python and TypeScript read and write the five new value types.** Python takes and returns `datetime.date`, `datetime.datetime` (naive is a local date-time, aware keeps its UTC offset), `bytes` (a `bytearray` or `memoryview` is accepted too) and the new `mushroomdb.Duration(months, days, nanos)`; a value that cannot be held exactly, such as an offset with a fractional second, raises `ValueError` rather than being rounded. TypeScript's `CellValue` gains the tagged forms `{ $date }`, `{ $ldt }`, `{ $dt }`, `{ $dur }` and `{ $binary }`, with `asDate`, `asLocalDateTime`, `asDateTime`, `asDuration`, `asBinary` and `binaryBytes` to narrow them.

### Fixed
- **Python `bytes` can be passed as a value.** A binary value read back as `bytes` but a write refused `bytes`, so it could not round-trip.
