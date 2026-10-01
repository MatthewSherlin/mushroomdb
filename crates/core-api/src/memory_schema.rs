//! The schema a memory store is created with.
//!
//! `recall` searches the pairs `fulltext_pairs()` reports, and until 0.7 the
//! only pair any memory store ever had was `Note.text`, self-declared by
//! `remember`. So `recall` could return notes and never the entities they were
//! about. This module is the answer: one `Schema` value, applied once at
//! creation, expressed entirely in types that already exist.
//!
//! It is deliberately *not* applied to an existing store on open. Declaring
//! full-text rebuilds the index at open — measured at 227 ms against 3.8 ms
//! with none (ledger row 36) — and creating a rule fires a backfill across
//! every node. An existing store is upgraded only by an explicit
//! `mushroomdb schema apply --memory-defaults`.

use crate::memory::identity::{ALIASES_FIELD, SAME_AS_EDGE, SAME_AS_FLOOR, SAME_AS_WEIGHT};
use crate::schema::Schema;
use core_rules::{Predicate, RuleDef};

/// Labels the memory surface treats as entities.
pub const MEMORY_ENTITY_LABELS: &[&str] = &["Person", "Org", "Project", "Concept", "Event"];

/// The label a provisional subject is created under by `remember`.
pub const PROVISIONAL_LABEL: &str = "Entity";

/// The property marking a node as provisional — a subject named before it was
/// described. Never removed silently; see the spec's O-4.
pub const PROVISIONAL_PROP: &str = "provisional";

/// The field each entity label carries its human-readable name in.
pub const NAME_FIELD: &str = "name";

/// The default schema a memory store is created with.
pub fn memory_defaults() -> Schema {
    let mut fulltext: Vec<(String, String)> = MEMORY_ENTITY_LABELS
        .iter()
        .map(|l| ((*l).to_string(), NAME_FIELD.to_string()))
        .collect();
    fulltext.push(("Note".to_string(), "text".to_string()));
    fulltext.push(("Concept".to_string(), "summary".to_string()));
    fulltext.push((PROVISIONAL_LABEL.to_string(), NAME_FIELD.to_string()));

    let indexes: Vec<(String, String)> = MEMORY_ENTITY_LABELS
        .iter()
        .map(|l| ((*l).to_string(), NAME_FIELD.to_string()))
        .collect();

    Schema {
        fulltext,
        indexes,
        // No rules: creating one stays an explicit, approved act
        // (SKILL.md:66). The identity rules are `memory_identity()`, opt-in.
        rules: Vec::new(),
        views: Vec::new(),
        roles: Vec::new(),
    }
}

/// The identity preset: `SAME_AS` rules over the `aliases` list.
///
/// Opt-in, never part of [`memory_defaults`]: a store gains rules only when
/// someone asks (`SKILL.md`: never create a rule silently), so this is applied
/// by `mushroomdb schema apply <db> --memory-identity` and nothing else.
///
/// Eleven rules, all global (`namespace: None`):
///
/// - one same-label rule per entity label and for the provisional label —
///   `Person→Person`, …, `Entity→Entity`. A same-label rule derives both
///   directions, which [`crate::memory::identity::same_as_pairs`] reads as one
///   claim;
/// - one directed rule from the provisional label to each entity label —
///   `Entity→Person`, …. A stub `remember` made from an `about` key is an
///   `Entity` for life, and a `Person→Person` rule never sees it, so without
///   these the case identity exists for — a subject named before it was
///   described — could never link.
///
/// `weight_prop` and `max_edges` are set explicitly: `apply_schema` takes a
/// `RuleDef` as given, without the defaults MCP `create_rule` fills in.
pub fn memory_identity() -> Schema {
    let predicate = Predicate::Overlap {
        field: ALIASES_FIELD.to_string(),
        min: SAME_AS_FLOOR,
    };
    let rule = |name: String, src: &str, dst: &str| RuleDef {
        name,
        src_label: src.to_string(),
        dst_label: dst.to_string(),
        predicate: predicate.clone(),
        edge_type: SAME_AS_EDGE.to_string(),
        weight_prop: Some(SAME_AS_WEIGHT.to_string()),
        max_edges: Some(core_rules::default_max_edges(&predicate)),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    };
    let mut rules: Vec<RuleDef> = MEMORY_ENTITY_LABELS
        .iter()
        .chain(std::iter::once(&PROVISIONAL_LABEL))
        .map(|l| rule(format!("same_as_{}", l.to_lowercase()), l, l))
        .collect();
    rules.extend(MEMORY_ENTITY_LABELS.iter().map(|l| {
        rule(
            format!("same_as_entity_{}", l.to_lowercase()),
            PROVISIONAL_LABEL,
            l,
        )
    }));
    Schema {
        fulltext: Vec::new(),
        indexes: Vec::new(),
        rules,
        views: Vec::new(),
        roles: Vec::new(),
    }
}
