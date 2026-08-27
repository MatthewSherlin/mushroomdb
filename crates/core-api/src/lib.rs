pub mod algo;
mod db;
mod ingest;
pub mod mask;
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
pub use db::{
    BatchBuilder, DeleteReport, EdgeInfo, Explanation, FsyncPolicy, GraphDb, MutationEvent,
    NodeInfo, NodeRef, PredicateSummary, RuleStats, SnapshotOptions, Stats,
};
pub use mask::NodeMask;
pub use ingest::{
    json_to_rows, json_to_value, AutoFk, FkSkip, IngestOptions, IngestReport, JsonRows,
};
pub use shared::SharedDb;
pub use subscription::{DbEvent, Subscription, DEFAULT_SUB_CAPACITY};

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
