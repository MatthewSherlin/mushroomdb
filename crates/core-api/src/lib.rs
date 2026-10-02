pub mod algo;
mod db;
pub mod digest;
mod exact_knn;
pub mod explain_digest;
pub mod history;
mod ingest;
pub mod mask;
pub mod memory;
pub mod memory_schema;
pub mod reader;
pub mod restore;
pub mod roles;
pub mod schema;
mod shared;
pub mod subscription;

pub use algo::{
    AlgoDir, Community, CommunityReport, DegreeConfig, DegreeReport, LouvainConfig, PageRankConfig,
    PageRankReport, WccConfig, WccReport,
};
pub use core_query::{CmpOp, Dir, Filter, ResultSet};
pub use core_rules::suggest::DEFAULT_SEED as SUGGEST_DEFAULT_SEED;
pub use core_rules::{
    default_max_edges, is_keymatch_rooted, AggFn, BuildProgress, Predicate, RuleDef,
    RuleSuggestion, SuggestConfig, SuggestReport, ViewDef, ViewSource, ViewStore,
    DEFAULT_KEYMATCH_TOP_K, DEFAULT_SCORED_TOP_K, HNSW_BUILD_BATCH, MAX_CHAIN_DEPTH,
};
pub use core_storage::fs::RealFs;
pub use core_storage::{Direction, GraphError, Result, Value};
pub use db::{
    ambiguous_exactness_last_warning, ambiguous_exactness_warns, ambiguous_exactness_warns_reset,
};
pub use db::{query_sub_exec_count, reset_query_sub_exec_count};
pub use db::{
    snapshot_version_at, valid_namespace, write_snapshot_bak, AsOfScope, BackupReport,
    BatchBuilder, BatchOp, BatchOutcome, DeleteReport, EdgeAt, EdgeInfo, Explanation, ExportEdge,
    FsyncPolicy, GraphDb, MaskedEdge, MaskedNodeResult, MutationEvent, NamespaceStats, NodeInfo,
    NodeRef, OnConflict, OpenOptions, Precondition, PredicateSummary, RuleStats, SlowQueryEntry,
    SlowQuerySnapshot, SnapshotOptions, Stats, WhatIf, WriteAuthz,
    MERGE_CREATE_NEEDS_ONE_NAMESPACE, NS_DEFAULT, NS_MAX_LEN, NS_PROP, WRITE_LOCK_WAIT,
};
pub use exact_knn::{with_pairwise_caps, PAIRWISE_GRAM_MAX, PAIRWISE_MAX_N};

/// The similarity floor `find_similar` applies when the caller names none.
///
/// One value, read by every surface that has a default — the MCP tool, HTTP
/// `POST /find_similar` and the Python binding. Through 0.6 each hard-coded
/// its own literal, and they disagreed: MCP 0.8, the other two 0.0, so a call
/// ported between them changed its results with nothing raised. The engine
/// itself has no default: `GraphDb::find_similar_vector_filtered` takes `min`
/// as an argument.
///
/// `pairwise_similar` is a different operation and defaults to 0.0 everywhere.
pub const FIND_SIMILAR_DEFAULT_MIN: f64 = 0.8;

/// The on-disk snapshot version a store that has opted in to nothing writes —
/// the **floor**, not the whole answer.
///
/// Exposed so CLI and tooling can print `V<SNAPSHOT_VERSION>` without depending
/// directly on `core-storage`.
///
/// Since v0.6.10 this build writes 9 **or** 10 depending on the store
/// (`core_storage::snapshot::version_for`: a store that has called
/// `GraphDb::enable_multiplicity` writes 10) and reads 5 through 10. Compare a
/// store's stamp against this with `>=`, never `==` — see `cli::run_migrate`,
/// which would otherwise report an opted-in store as `V10 -> V9`.
pub const SNAPSHOT_VERSION: u16 = core_storage::snapshot::VERSION;
pub use history::{EdgeEvent, EdgeHistoryEvent, HistoryChange, HistoryEntry, HistoryResult};
pub use ingest::{
    json_to_rows, json_to_value, AutoFk, FkSkip, IngestOptions, IngestReport, JsonRows,
};
pub use mask::{MaskMode, NodeMask, RoleMaskCache, Scope};
pub use reader::{CommitDelta, FrozenOverlay, ReaderSnapshot, FOLD_EVERY_K};
pub use roles::{PropPredicate, RoleDef, WriteScope};
pub use schema::{Schema, SchemaDiff};
pub use shared::{SharedDb, WriteGuard};
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
