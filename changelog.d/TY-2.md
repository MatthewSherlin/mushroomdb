### Changed

- **Rust: Date, LocalDateTime, DateTime, Duration and Binary values now have typed in-memory columns** instead of the generic mixed-value path. Nothing user-visible yet: these values still cannot be stored until the V11 format lands, and the new column pack tags (5-9) are written only by a V11 snapshot.
