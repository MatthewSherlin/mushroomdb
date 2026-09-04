//! Rule definitions more than one write path needs to agree on.
//!
//! `structure::ensure_rules_and_fulltext` (CLI, `ingest-git`/`sync`) and
//! [`remember`](super::remember::remember) both write nodes whose derived
//! edges depend on an `about_<label>` rule existing — the first as part of a
//! full refresh, the second one `Note` at a time, possibly the very first
//! write a store ever sees. Two independent copies of these definitions
//! would eventually drift; this module is the one place they are declared,
//! so both callers create byte-identical [`RuleDef`]s under the same names.

use core_rules::{default_max_edges, Predicate, RuleDef};

/// Labels a `Note.about` entry may point at, in rule-creation order. One
/// `about_<label>` rule per label, since a rule has a single destination.
pub const ABOUT_LABELS: [&str; 5] = ["Author", "Concept", "File", "Note", "Symbol"];

/// The `about_<label>` rule for one of [`ABOUT_LABELS`]: `Note.about` →
/// `ABOUT` edges to `label`.
#[must_use]
pub fn about_rule(label: &str) -> RuleDef {
    key_rule(
        &format!("about_{}", label.to_lowercase()),
        "Note",
        label,
        "about",
        "ABOUT",
    )
}

/// `Concept.source_files` → `DESCRIBED_IN` edges to `File`, the rule that
/// backs [`stale_concepts`](super::stale_concepts)'s provenance.
#[must_use]
pub fn concept_sources_rule() -> RuleDef {
    key_rule(
        "concept_sources",
        "Concept",
        "File",
        "source_files",
        "DESCRIBED_IN",
    )
}

/// A `KeyMatch` rule with the engine's default fan-out for the predicate,
/// stated rather than left implicit — the convention across this crate.
fn key_rule(name: &str, src: &str, dst: &str, field: &str, edge: &str) -> RuleDef {
    let predicate = Predicate::KeyMatch {
        field: field.into(),
    };
    let max_edges = Some(default_max_edges(&predicate));
    RuleDef {
        name: name.into(),
        src_label: src.into(),
        dst_label: dst.into(),
        predicate,
        edge_type: edge.into(),
        weight_prop: None,
        max_edges,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn about_rule_names_and_shapes_match_one_label_per_call() {
        let r = about_rule("Concept");
        assert_eq!(r.name, "about_concept");
        assert_eq!(r.src_label, "Note");
        assert_eq!(r.dst_label, "Concept");
        assert_eq!(r.edge_type, "ABOUT");
        assert_eq!(r.max_edges, Some(default_max_edges(&r.predicate)));
    }

    #[test]
    fn concept_sources_rule_is_concept_to_file() {
        let r = concept_sources_rule();
        assert_eq!(r.name, "concept_sources");
        assert_eq!(r.src_label, "Concept");
        assert_eq!(r.dst_label, "File");
        assert_eq!(r.edge_type, "DESCRIBED_IN");
    }
}
