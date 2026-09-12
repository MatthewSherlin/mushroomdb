pub mod def;
pub mod engine;
pub mod hnsw;
pub mod index;
pub mod suggest;
pub mod views;
pub use def::{
    decode_rule_def, default_max_edges, evaluate, is_keymatch_rooted, NodeView, Predicate, RuleDef,
    DEFAULT_KEYMATCH_TOP_K, DEFAULT_SCORED_TOP_K, MAX_KEYMATCH_LIST,
};
pub use engine::{
    BuildProgress, EngineEdgeDelta, GraphMut, RuleEngine, RuleIvfExport, SideIvfExport,
    MAX_CHAIN_DEPTH,
};
pub use hnsw::HnswIndex;
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub use hnsw::{
    hnsw_dist_evals, hnsw_dist_evals_pairwise, hnsw_dist_evals_reset, hnsw_insert_count,
    hnsw_insert_count_reset, hnsw_remove_scanned, hnsw_remove_scanned_reset, hnsw_search_count,
    hnsw_search_count_reset,
};
pub use index::{
    candidate_spec, hnsw_vector_present, vector_scan_forced, with_ef_max, with_hnsw_build_batch,
    with_ivf_drift_rebuild, with_vector_scan, CandidateSpec, RuleIndex, SideIndex, EF_MAX,
    HNSW_BUILD_BATCH, IVF_DRIFT_REBUILD,
};
pub use suggest::{
    RuleSuggestion, SuggestConfig, SuggestReport, DEFAULT_SEED as SUGGEST_DEFAULT_SEED,
};
pub use views::{AggFn, ViewDef, ViewSource, ViewStore};
