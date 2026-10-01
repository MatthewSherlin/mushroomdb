//! The store's own rule proposals, made fit to relay.
//!
//! [`crate::GraphDb::suggest_rules_with_config`] profiles the store and
//! proposes rules. Two things stand between that and something a person
//! should be shown, and both live here so that every surface shows the same
//! thing: proposals over fields the store writes for its own bookkeeping are
//! dropped, and each survivor carries the arguments that create it —
//! arguments `create_rule` accepts unchanged over MCP and in the Python
//! binding, and that create the same rule on both.

use crate::explain_digest::predicate_summary;
use crate::memory::forget::predicate_fields;
use crate::{GraphDb, PredicateSummary, RuleDef, SuggestConfig, SUGGEST_DEFAULT_SEED};
use core_storage::fs::Fs;
use serde_json::{json, Value as Js};

/// Fields the store writes for its own bookkeeping, never proposed as a rule.
///
/// On a memory store these are the whole of what the engine would otherwise
/// propose: measured on a 100,000-node store `remember` filled, every proposal
/// was `kind`, `source` or `ts` — clique rules that link every note to 32
/// others for sharing a default value. `ns` is the namespace every namespaced
/// node carries, `id` repeats the key, `provisional` is `remember`'s stub mark,
/// `aliases` is the list `remember` maintains for identity matching and
/// `alias_keys` is the list of aliases a caller declared, kept as written.
pub const BOOKKEEPING_FIELDS: [&str; 8] = [
    "ns",
    "kind",
    "ts",
    "source",
    "provisional",
    "id",
    "aliases",
    "alias_keys",
];

/// `x` rounded to `places` decimals: a report's floats at the precision its
/// text prints them, so the two agree and a re-run is byte-identical.
#[must_use]
pub fn round_to(x: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (x * scale).round() / scale
}

/// `def` as `create_rule` arguments: no nulls, `approximate` only when set,
/// and `weight_prop` explicit — the MCP tool would default a missing one to
/// `"weight"` and the Python binding would leave it unset, so it is written
/// out to make what is shown what is created, on both.
#[must_use]
pub fn create_rule_args(def: &RuleDef) -> Js {
    let mut v = serde_json::to_value(def).unwrap_or(Js::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.retain(|_, x| !x.is_null());
        if obj.get("approximate") == Some(&Js::Bool(false)) {
            obj.remove("approximate");
        }
        obj.entry("weight_prop").or_insert_with(|| json!("weight"));
    }
    v
}

/// One proposal, with the arguments that create it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Suggestion {
    pub name: String,
    pub src_label: String,
    pub dst_label: String,
    pub edge_type: String,
    /// The predicate in one clause, as `schema` prints it.
    pub predicate: String,
    pub est_edges: u64,
    /// `(src_key, dst_key, score)`, scores at two decimals.
    pub examples: Vec<(String, String, f64)>,
    pub rationale: String,
    /// Pass this object to `create_rule` as its arguments, unchanged.
    pub create_rule_args: Js,
}

/// [`filtered_suggestions`]' answer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FilteredSuggestions {
    /// Every proposal that reads no bookkeeping field, in the engine's order
    /// (estimated edges, descending). Not capped: a caller that relays them
    /// to a person cuts the list itself.
    pub suggestions: Vec<Suggestion>,
    /// `suggestions.len()`, kept as a field so a caller that cuts the list
    /// can still say how many there were.
    pub total: usize,
    /// Proposals dropped because they read a bookkeeping field.
    pub bookkeeping_hidden: usize,
    /// The engine's time budget ran out; a second call may find more.
    pub truncated: bool,
}

/// Profile `db` with the default configuration and seed, and return what is
/// worth proposing. Creates nothing: a rule is never created silently.
pub fn filtered_suggestions<F: Fs>(db: &GraphDb<F>) -> FilteredSuggestions {
    let report = db.suggest_rules_with_config(&SuggestConfig::default(), SUGGEST_DEFAULT_SEED);
    let mut kept: Vec<Suggestion> = Vec::new();
    let mut hidden = 0usize;
    for s in report.suggestions {
        let summary = PredicateSummary::from(&s.def.predicate);
        if predicate_fields(&summary)
            .iter()
            .any(|f| BOOKKEEPING_FIELDS.contains(&f.as_str()))
        {
            hidden += 1;
            continue;
        }
        kept.push(Suggestion {
            create_rule_args: create_rule_args(&s.def),
            predicate: predicate_summary(&summary),
            name: s.def.name,
            src_label: s.def.src_label,
            dst_label: s.def.dst_label,
            edge_type: s.def.edge_type,
            est_edges: s.est_edges,
            // Fixed precision in the report too, not only in rendered text:
            // a determinism claim covers both.
            examples: s
                .examples
                .into_iter()
                .map(|(a, b, score)| (a, b, round_to(score, 2)))
                .collect(),
            rationale: s.rationale,
        });
    }
    FilteredSuggestions {
        total: kept.len(),
        suggestions: kept,
        bookkeeping_hidden: hidden,
        truncated: report.truncated,
    }
}
