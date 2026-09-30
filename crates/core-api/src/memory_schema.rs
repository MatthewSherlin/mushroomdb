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

use crate::schema::Schema;

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
        // SAME_AS arrives in plan 3; a memory store ships with no rules so
        // that creating one stays an explicit, approved act (SKILL.md:74).
        rules: Vec::new(),
        views: Vec::new(),
        roles: Vec::new(),
    }
}
