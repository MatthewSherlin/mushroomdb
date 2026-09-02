pub mod algo;
mod db;
pub mod history;
mod ingest;
pub mod mask;
pub mod reader;
pub mod roles;
pub mod schema;
mod shared;
pub mod subscription;

pub use algo::{
    AlgoDir, DegreeConfig, DegreeReport, PageRankConfig, PageRankReport, WccConfig, WccReport,
};
pub use core_query::{CmpOp, Dir, Filter, ResultSet};
pub use core_rules::suggest::DEFAULT_SEED as SUGGEST_DEFAULT_SEED;
pub use core_rules::{
    default_max_edges, is_keymatch_rooted, AggFn, Predicate, RuleDef, RuleSuggestion,
    SuggestConfig, SuggestReport, ViewDef, ViewSource, ViewStore, DEFAULT_KEYMATCH_TOP_K,
    DEFAULT_SCORED_TOP_K,
};
pub use core_storage::{Direction, GraphError, Result, Value};
pub use db::{query_sub_exec_count, reset_query_sub_exec_count};
pub use db::{
    snapshot_version_at, write_snapshot_bak, BackupReport, BatchBuilder, BatchOp, DeleteReport,
    EdgeInfo, Explanation, ExportEdge, FsyncPolicy, GraphDb, MaskedEdge, MaskedNodeResult,
    MutationEvent, NodeInfo, NodeRef, OpenOptions, Precondition, PredicateSummary, RuleStats,
    SlowQueryEntry, SlowQuerySnapshot, SnapshotOptions, Stats, WriteAuthz,
};

/// Current on-disk snapshot format version written by this build.
///
/// Exposed so CLI and tooling can print `V<SNAPSHOT_VERSION>` without depending
/// directly on `core-storage`.
pub const SNAPSHOT_VERSION: u16 = core_storage::snapshot::VERSION;
pub use history::{EdgeEvent, EdgeHistoryEvent, HistoryChange, HistoryEntry, HistoryResult};
pub use ingest::{
    json_to_rows, json_to_value, AutoFk, FkSkip, IngestOptions, IngestReport, JsonRows,
};
pub use mask::{MaskMode, NodeMask};
pub use reader::{CommitDelta, FrozenOverlay, ReaderSnapshot, FOLD_EVERY_K};
pub use roles::{RoleDef, WriteScope};
pub use schema::{Schema, SchemaDiff};
pub use shared::SharedDb;
pub use subscription::{DbEvent, Subscription, DEFAULT_SUB_CAPACITY};

/// One verification entry per section: `(section_id, section_name, bytes_checked, result)`.
///
/// Returned by [`verify_snapshot`].
pub type SectionVerifyResult = (u8, &'static str, usize, std::result::Result<(), String>);

/// Validate the CRC32 integrity of every section in the V8 snapshot at `dir`.
///
/// Returns one entry per section directory entry (see [`SectionVerifyResult`]).
///
/// Large sections (TOPOLOGY, COLUMNS, EDGE_PROPS, HNSW, PROVENANCE, IVF_STATE)
/// skip CRC on the normal hot query path; this function always checks them.
/// Use it to implement `mushroomdb verify` without depending on `core-storage`
/// directly.
pub fn verify_snapshot(dir: &std::path::Path) -> crate::Result<Vec<SectionVerifyResult>> {
    let snap_path = dir.join("snapshot.bin");
    let mapped = core_storage::v8::MappedBase::map(&snap_path)?;
    // Bounds first, then per-section CRC32, then a structural (rkyv bytecheck)
    // pass over the sections the hot path reads unchecked. The structural pass
    // rejects a maliciously crafted snapshot whose relative pointers would
    // otherwise trigger UB on open — a threat CRC32 alone can't catch (an
    // attacker can recompute the CRC). Fail loud on structural corruption.
    mapped.validate_section_bounds()?;
    let results = mapped.verify_integrity();
    mapped.validate_hot_sections()?;
    Ok(results)
}

/// Return `true` if `cypher` is a write statement (CREATE / MERGE / MATCH…SET /
/// MATCH…DELETE).  Returns `Err` only when the string fails to lex.
///
/// Used by the HTTP server to dispatch to the write lock without a full parse.
pub fn is_write_query(cypher: &str) -> std::result::Result<bool, String> {
    let toks = core_query::cypher::lex(cypher).map_err(|e| format!("lex: {e}"))?;
    Ok(core_query::cypher::is_write_tokens(&toks))
}

/// Return the number of valid WAL commits in the database at `dir`.
///
/// Useful for displaying "as-of commit N of M" in CLIs without opening the
/// full database.  Returns 0 if the WAL file does not exist (e.g., after
/// `snapshot()` which truncates it to empty).
pub fn wal_commit_count_at(dir: &std::path::Path) -> crate::Result<u64> {
    let wal_path = dir.join("wal.bin");
    let bytes = match std::fs::read(&wal_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(core_storage::GraphError::Io(e)),
    };
    Ok(core_storage::wal::wal_commits(&bytes))
}
