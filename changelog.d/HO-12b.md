### Fixed
- **`serve` on a data directory it cannot write now says so, instead of "store is busy: write lock held".** The error names the path, the process's uid, the directory's owner and mode, and what to change. A genuinely held lock still reports `busy`. `serve` refuses to start on such a directory.
- **Rust: `GraphError` has a new variant, `Unwritable { path, detail }`**, so an exhaustive `match` on it needs a new arm. Python raises it as `Unwritable` (code `unwritable`); HTTP answers 500 with a fixed message; Bolt maps it to a database error without the path.
- **A volume-ownership recipe for Fly, Render, Railway and Compose** in the deployment guide, and in the Fly and Render templates. These are confirmed by a real deploy at the release gate.
