mod db;
mod ingest;
mod shared;

pub use core_query::{CmpOp, Dir, Filter, ResultSet};
pub use core_rules::{Predicate, RuleDef};
pub use core_storage::{Direction, GraphError, Result, Value};
pub use db::{
    BatchBuilder, DeleteReport, EdgeInfo, Explanation, GraphDb, MutationEvent, NodeInfo, NodeRef,
    PredicateSummary, RuleStats, Stats,
};
pub use ingest::{
    json_to_rows, json_to_value, AutoFk, FkSkip, IngestOptions, IngestReport, JsonRows,
};
pub use shared::SharedDb;

/// Return `true` if `cypher` is a write statement (CREATE / MERGE / MATCH…SET /
/// MATCH…DELETE).  Returns `Err` only when the string fails to lex.
///
/// Used by the HTTP server to dispatch to the write lock without a full parse.
pub fn is_write_query(cypher: &str) -> std::result::Result<bool, String> {
    let toks = core_query::cypher::lex(cypher).map_err(|e| format!("lex: {e}"))?;
    Ok(core_query::cypher::is_write_tokens(&toks))
}
