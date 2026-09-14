//! Documented refusal strings are pinned to the source that emits them.
//!
//! Each row is a backticked or fenced refusal from `docs/site/{api,masks,rules,mcp}.md`
//! and the `.rs` file that produces that text. Interpolated examples pin the
//! constant parts that appear in both. A missing pair names the doc, the
//! string, and the source file.

use std::fs;
use std::path::PathBuf;

/// `(doc file, needle, source file)`.
///
/// `doc` is a basename under `docs/site/`. `source` is relative to `crates/`.
const PAIRS: &[(&str, &str, &str)] = &[
    // docs/site/api.md
    (
        "api.md",
        "role-bound token: MERGE create requires the role to name one namespace",
        "core-api/src/db.rs",
    ),
    (
        "api.md",
        "role-bound token: edge endpoint not visible",
        "core-api/src/db.rs",
    ),
    (
        "api.md",
        "as_of (time-travel) does not compose with stub_hidden",
        "server/src/http.rs",
    ),
    (
        "api.md",
        "role-bound token: target node not visible",
        "core-api/src/db.rs",
    ),
    (
        "api.md",
        "role-bound token: this endpoint is not permitted",
        "core-api/src/db.rs",
    ),
    (
        "api.md",
        "not in write scope (create_labels)",
        "core-api/src/db.rs",
    ),
    (
        "api.md",
        "not in write scope (create_edge_types)",
        "core-api/src/db.rs",
    ),
    (
        "api.md",
        "role-bound token: namespace '",
        "core-api/src/db.rs",
    ),
    ("api.md", "node key not found", "core-storage/src/types.rs"),
    (
        "api.md",
        "is out of range; valid range is ",
        "core-storage/src/types.rs",
    ),
    (
        "api.md",
        "events before commit ",
        "core-storage/src/types.rs",
    ),
    (
        "api.md",
        "snapshot: unsupported version ",
        "core-storage/src/snapshot.rs",
    ),
    // docs/site/masks.md
    (
        "masks.md",
        "ns is given more than once; a node has exactly one namespace",
        "core-query/src/cypher/parser.rs",
    ),
    ("masks.md", "given more than once", "core-api/src/db.rs"),
    (
        "masks.md",
        "role-bound token: edge endpoint not visible",
        "core-api/src/db.rs",
    ),
    (
        "masks.md",
        "role-bound token: MERGE create requires the role to name one namespace",
        "core-api/src/db.rs",
    ),
    (
        "masks.md",
        "role-bound token: /stats requires a full-access token",
        "server/src/http.rs",
    ),
    (
        "masks.md",
        "as_of (time-travel) does not compose with stub_hidden",
        "server/src/http.rs",
    ),
    (
        "masks.md",
        "role-bound token: namespace '",
        "core-api/src/db.rs",
    ),
    (
        "masks.md",
        "a namespace is set at insert and cannot",
        "core-storage/src/types.rs",
    ),
    (
        "masks.md",
        "delete and re-insert the node instead",
        "core-storage/src/types.rs",
    ),
    (
        "masks.md",
        "crosses a namespace boundary (",
        "core-storage/src/types.rs",
    ),
    (
        "masks.md",
        "only a global rule may derive one",
        "core-storage/src/types.rs",
    ),
    (
        "masks.md",
        "is not a valid namespace name — 1 to ",
        "server/src/json.rs",
    ),
    // docs/site/rules.md
    (
        "rules.md",
        "predicate nesting depth ",
        "core-rules/src/def.rs",
    ),
    // docs/site/mcp.md
    (
        "mcp.md",
        "ambiguous target labels",
        "core-api/src/ingest.rs",
    ),
    (
        "mcp.md",
        "pass one of all_of or edge_type, not",
        "server/src/mcp_tasks.rs",
    ),
];

fn read_doc(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/site")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading doc {name} at {}: {e}", path.display()))
}

fn source_text(path: &str) -> &'static str {
    match path {
        "core-api/src/db.rs" => include_str!("../../core-api/src/db.rs"),
        "core-api/src/ingest.rs" => include_str!("../../core-api/src/ingest.rs"),
        "core-storage/src/types.rs" => include_str!("../../core-storage/src/types.rs"),
        "core-storage/src/snapshot.rs" => include_str!("../../core-storage/src/snapshot.rs"),
        "core-query/src/cypher/parser.rs" => include_str!("../../core-query/src/cypher/parser.rs"),
        "core-rules/src/def.rs" => include_str!("../../core-rules/src/def.rs"),
        "server/src/http.rs" => include_str!("../src/http.rs"),
        "server/src/json.rs" => include_str!("../src/json.rs"),
        "server/src/mcp_tasks.rs" => include_str!("../src/mcp_tasks.rs"),
        other => panic!("unmapped source file {other}"),
    }
}

#[test]
fn every_documented_refusal_string_exists_in_the_source() {
    for &(doc, needle, source) in PAIRS {
        let docs = read_doc(doc);
        assert!(
            docs.contains(needle),
            "refusal {needle:?} missing from docs/site/{doc} (source {source})"
        );
        let src = source_text(source);
        assert!(
            src.contains(needle),
            "refusal {needle:?} from docs/site/{doc} missing from {source}"
        );
    }
}
