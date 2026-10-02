//! `forget` — tombstone a node, remove one property, or retract one fact, and
//! say what is left behind.
//!
//! The decision and the report live here so that every surface gives the
//! same answer: the MCP `forget` tool renders [`ForgetReport`] as prose, and
//! the Python binding returns it as a dict. Before 0.7's last plan this was
//! private to the MCP server.
//!
//! A tombstone is not a redaction. History still holds what was forgotten
//! until the log is pruned, and a note that stated a forgotten fact still
//! says it: the report lists those notes and deletes none of them.

use crate::digest::sanitize;
use crate::memory::identity::{
    identity_props_after_forgetting_name, ALIASES_FIELD, ALIAS_KEYS_FIELD,
};
use crate::memory_schema::NAME_FIELD;
use crate::{GraphDb, PredicateSummary, RuleDef};
use core_rules::engine::DEFAULT_MAX_EDGES;
use core_rules::{evaluate, NodeView};
use core_storage::fs::Fs;
use core_storage::{namespace_of_value, Direction, GraphError, Result, NS_PROP};
use std::collections::BTreeSet;

/// Notes a report names when what it forgot was written about; the rest are
/// counted in [`ForgetReport::notes_total`].
pub const FORGET_NOTE_LIST: usize = 10;

/// The refusal for a call that is not exactly one of the three shapes.
pub const FORGET_SHAPE: &str = "pass exactly one of: key (forget a node), key and prop \
     (forget one property), or fact {subject, predicate, object} (retract one edge)";

/// What to forget: exactly one of a node, one property, or one fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetTarget {
    /// Tombstone the node and every edge on it.
    Node { key: String },
    /// Remove one property from a node.
    Prop { key: String, prop: String },
    /// Retract one hand-written edge, `(predicate, subject, object)`.
    Fact {
        subject: String,
        predicate: String,
        object: String,
    },
}

impl ForgetTarget {
    /// The target three optional arguments name, or `None` when they are not
    /// exactly one of the three shapes — the caller answers [`FORGET_SHAPE`].
    #[must_use]
    pub fn from_parts(
        key: Option<String>,
        prop: Option<String>,
        fact: Option<(String, String, String)>,
    ) -> Option<Self> {
        match (key, prop, fact) {
            (Some(key), None, None) => Some(Self::Node { key }),
            (Some(key), Some(prop), None) => Some(Self::Prop { key, prop }),
            (None, None, Some((subject, predicate, object))) => Some(Self::Fact {
                subject,
                predicate,
                object,
            }),
            _ => None,
        }
    }
}

/// What one `forget` call did.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ForgetReport {
    /// `node`, `prop` or `fact`.
    pub mode: &'static str,
    /// The node, property or edge named, as the caller named it.
    pub target: String,
    /// False when there was nothing to forget, and nothing was written.
    pub changed: bool,
    /// Edges removed with a node: written by hand, and derived by rules. For a
    /// property, `derived_edges` counts the rule-derived edges on the node that
    /// the removal retracted, because a rule read that property.
    pub manual_edges: u64,
    pub derived_edges: u64,
    /// The property removed, in `prop` mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prop: Option<String>,
    /// Notes with an `ABOUT` edge to the node, or to both ends of the fact.
    /// Listed, never deleted: their text still says what was forgotten.
    pub notes: Vec<String>,
    pub notes_total: usize,
    /// The first commit history still answers from.
    pub history_floor: u64,
    /// `prop` mode removed `name` from a node that carries `aliases`, and
    /// rewrote that list in the same write: the name's words left it.
    pub aliases_rewritten: bool,
    /// `prop` mode removed `aliases` from a node that still carries
    /// `alias_keys`, the aliases it declared, which still link stubs.
    pub alias_keys_remain: bool,
}

/// Every field a predicate reads, its parts' included, sorted and deduped.
#[must_use]
pub fn predicate_fields(p: &PredicateSummary) -> Vec<String> {
    let mut out: BTreeSet<String> = p.fields.iter().cloned().collect();
    for part in p.parts.iter().flatten() {
        out.extend(predicate_fields(part));
    }
    out.into_iter().collect()
}

/// Notes with an `ABOUT` edge to every one of `keys`, sorted.
fn notes_about<F: Fs>(db: &GraphDb<F>, keys: &[&str]) -> Vec<String> {
    let mut common: Option<BTreeSet<String>> = None;
    for key in keys {
        let into: BTreeSet<String> = db
            .neighbors(key, "ABOUT", Direction::In)
            .unwrap_or_default()
            .into_iter()
            .filter(|k| db.node_ref(k).is_some_and(|n| n.label() == "Note"))
            .collect();
        common = Some(match common {
            None => into,
            Some(c) => c.intersection(&into).cloned().collect(),
        });
    }
    common.unwrap_or_default().into_iter().collect()
}

/// An edge as `(edge_type, src_key, dst_key)`.
type EdgeTriple = (String, String, String);

/// The rule-derived edges incident on `key`, both directions, each once.
fn derived_edges_on<F: Fs>(db: &GraphDb<F>, key: &str) -> Result<BTreeSet<EdgeTriple>> {
    Ok(db
        .node_edges(key)?
        .into_iter()
        .filter(|e| e.derived)
        .map(|e| (e.edge_type, e.src_key, e.dst_key))
        .collect())
}

/// Forget `target`, in one call on the one handle, so nothing can change
/// between what the report says and what was done.
///
/// # Errors
/// - [`GraphError::KeyNotFound`] for a node or a fact endpoint that does not
///   exist. Nothing is written.
/// - [`GraphError::RuleOwned`] for a fact that is in the store and that the
///   engine will not delete: a rule derived it, or it was written by hand
///   and a rule matches the pair. `detail` is the whole refusal
///   ([`rule_owned_refusal`]): which of the two, the rule and the fields it
///   reads, or, when no rule can be named, the engine's own detail,
///   sanitized. Nothing is written. A fact that is not in the store is never
///   this error: it is `changed: false`, whatever a rule says of the pair.
/// - Any other engine refusal, as it came.
pub fn forget<F: Fs>(db: &mut GraphDb<F>, target: &ForgetTarget) -> Result<ForgetReport> {
    match target {
        ForgetTarget::Node { key } => {
            if !db.has_node(key) {
                return Err(GraphError::KeyNotFound { key: key.clone() });
            }
            let notes = notes_about(db, &[key.as_str()]);
            let label = db
                .node_ref(key)
                .map(|n| n.label().to_string())
                .unwrap_or_default();
            let deleted = db.delete_node(key)?;
            Ok(ForgetReport {
                mode: "node",
                target: format!("{key} ({label})"),
                changed: true,
                manual_edges: deleted.manual_edges,
                derived_edges: deleted.derived_edges,
                prop: None,
                notes_total: notes.len(),
                notes: notes.into_iter().take(FORGET_NOTE_LIST).collect(),
                history_floor: db.stats().history_floor,
                aliases_rewritten: false,
                alias_keys_remain: false,
            })
        }
        ForgetTarget::Prop { key, prop } => {
            // Removing a property re-runs rule retraction, so a rule that read
            // it drops the edges it derived. Compare the node's derived edges
            // either side of the write to say how many went.
            let before = derived_edges_on(db, key)?;
            // A name's words live in `aliases`. When the name goes, the list is
            // rewritten from the key alone in the same commit, so the words
            // leave with it. A node with no `aliases` list, or one holding a
            // value the store cannot read as a list, is left as it is.
            let rewrite = if prop == NAME_FIELD && db.get_prop(key, prop).is_some() {
                identity_props_after_forgetting_name(db, key).unwrap_or_default()
            } else {
                Vec::new()
            };
            let changed = if rewrite.is_empty() {
                db.remove_prop(key, prop)?
            } else {
                let mut batch = db.batch();
                batch.remove_prop(key, prop);
                for (field, value) in &rewrite {
                    batch.set_prop(key, field, value.clone());
                }
                batch.commit().map(|_| true)?
            };
            let aliases_rewritten = changed && !rewrite.is_empty();
            let alias_keys_remain =
                changed && prop == ALIASES_FIELD && db.get_prop(key, ALIAS_KEYS_FIELD).is_some();
            let retracted = if changed {
                let after = derived_edges_on(db, key).unwrap_or_default();
                before.difference(&after).count() as u64
            } else {
                0
            };
            Ok(ForgetReport {
                mode: "prop",
                target: format!("{key}.{prop}"),
                changed,
                manual_edges: 0,
                derived_edges: retracted,
                prop: Some(prop.clone()),
                notes: Vec::new(),
                notes_total: 0,
                history_floor: db.stats().history_floor,
                aliases_rewritten,
                alias_keys_remain,
            })
        }
        ForgetTarget::Fact {
            subject,
            predicate,
            object,
        } => {
            let changed = match db.delete_edge(predicate, subject, object) {
                Ok(c) => c,
                // The engine's guard refuses before it looks for the edge
                // (ledger row 67), so a fact nobody stated can come back
                // `RuleOwned`. An edge that is not there is nothing to
                // retract, whatever a rule says about the pair.
                Err(GraphError::RuleOwned { .. }) if !has_edge(db, predicate, subject, object) => {
                    false
                }
                Err(GraphError::RuleOwned { detail }) => {
                    return Err(GraphError::RuleOwned {
                        detail: rule_owned_refusal(db, predicate, subject, object, &detail),
                    })
                }
                Err(e) => return Err(e),
            };
            let notes = if changed {
                notes_about(db, &[subject.as_str(), object.as_str()])
            } else {
                Vec::new()
            };
            Ok(ForgetReport {
                mode: "fact",
                target: format!("{predicate} {subject} → {object}"),
                changed,
                manual_edges: 0,
                derived_edges: 0,
                prop: None,
                notes_total: notes.len(),
                notes: notes.into_iter().take(FORGET_NOTE_LIST).collect(),
                history_floor: db.stats().history_floor,
                aliases_rewritten: false,
                alias_keys_remain: false,
            })
        }
    }
}

/// Whether the edge `(predicate, subject, object)` is in the store.
fn has_edge<F: Fs>(db: &GraphDb<F>, predicate: &str, subject: &str, object: &str) -> bool {
    db.neighbors(subject, predicate, Direction::Out)
        .is_ok_and(|keys| keys.iter().any(|k| k == object))
}

/// Why a fact edge that exists cannot be retracted, in a sentence that is
/// true of this edge.
///
/// The engine refuses the delete in two cases, and they are not the same
/// fact:
///
/// - **A rule derived the edge.** It is in provenance, and the refusal names
///   exactly the rules [`GraphDb::explain`] attributes it to.
/// - **The edge was written by hand and a rule matches the pair.** It is in
///   no provenance, so no rule is said to have derived it. The refusal names
///   the rules the engine's guard refused for — `guard_matches` below, the
///   same test — and not every rule that shares the edge type and the
///   endpoint labels. It says those rules *would derive it again* only when
///   every one of them would (`would_derive`); the guard asks less than
///   that, so otherwise it says they match the two nodes' properties and may.
///
/// When neither names a rule, the engine's own `detail` is returned,
/// sanitized.
///
/// [`forget`] calls this only for an edge that is in the store: an absent one
/// is nothing to retract, and "was written by hand" would be false of it.
pub fn rule_owned_refusal<F: Fs>(
    db: &GraphDb<F>,
    predicate: &str,
    subject: &str,
    object: &str,
    detail: &str,
) -> String {
    // Which rule derived this edge is something the engine knows: `explain`
    // answers from provenance, one entry per derived edge between the two
    // keys. Matching on edge type and labels alone names every look-alike —
    // every rule of the identity preset derives `SAME_AS`.
    let by_provenance: BTreeSet<String> = db
        .explain(subject, object)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.edge_type == predicate && e.src_key == subject && e.dst_key == object)
        .map(|e| e.rule)
        .collect();
    let rules = db.rules();
    let derived_by: Vec<&RuleDef> = rules
        .iter()
        .filter(|r| by_provenance.contains(&r.name))
        .collect();
    if !derived_by.is_empty() {
        let (names, fields) = names_and_fields(&derived_by);
        return format!(
            "refused: {} {} → {} is derived by rule {names}. It changes only when the fields \
             that rule reads change ({fields}), or when the rule is deleted. Nothing was written.",
            sanitize(predicate),
            sanitize(subject),
            sanitize(object),
        );
    }
    let matching: Vec<&RuleDef> = rules
        .iter()
        .filter(|r| guard_matches(db, r, predicate, subject, object))
        .collect();
    if matching.is_empty() {
        return sanitize(detail);
    }
    let (names, fields) = names_and_fields(&matching);
    // One claim for the whole list, so it has to hold for every rule in it.
    let claim = if matching
        .iter()
        .all(|r| would_derive(db, r, subject, object))
    {
        "would derive it again"
    } else {
        "matches these two nodes' properties and may derive it again"
    };
    format!(
        "refused: {} {} → {} was written by hand and no rule derived it, but rule {names} \
         {claim}, so the delete is refused. It can be deleted once the fields that rule reads \
         ({fields}) no longer match, or once the rule is deleted. Nothing was written.",
        sanitize(predicate),
        sanitize(subject),
        sanitize(object),
    )
}

/// The rules' names, and every field their predicates read, each sanitized
/// and comma-joined; the fields sorted and deduped.
fn names_and_fields(rules: &[&RuleDef]) -> (String, String) {
    let names: Vec<String> = rules.iter().map(|r| sanitize(&r.name)).collect();
    let mut fields: BTreeSet<String> = BTreeSet::new();
    for r in rules {
        fields.extend(predicate_fields(&PredicateSummary::from(&r.predicate)));
    }
    let fields: Vec<String> = fields.iter().map(|f| sanitize(f)).collect();
    (names.join(", "), fields.join(", "))
}

/// Whether `rule`'s predicate holds between the nodes `a` and `b`.
fn predicate_holds<F: Fs>(db: &GraphDb<F>, rule: &RuleDef, a: &str, b: &str) -> bool {
    let a_props = |field: &str| db.get_prop(a, field);
    let b_props = |field: &str| db.get_prop(b, field);
    evaluate(
        &rule.predicate,
        &NodeView {
            key: a,
            props: &a_props,
        },
        &NodeView {
            key: b,
            props: &b_props,
        },
    )
    .is_some()
}

/// Whether the engine's delete guard refuses `(predicate, subject, object)`
/// on account of `rule`.
///
/// A mirror of the private `would_derive` in `db.rs`, which is why a
/// hand-written edge comes back `RuleOwned`: the rule's edge type and
/// endpoint labels, then its predicate evaluated on the pair. It is all the
/// guard asks. It does not look at the rule's via hop, its namespace or its
/// per-source cap (ledger row 67), so a rule can match here and still not be
/// one that would derive the edge — see [`would_derive`].
fn guard_matches<F: Fs>(
    db: &GraphDb<F>,
    rule: &RuleDef,
    predicate: &str,
    subject: &str,
    object: &str,
) -> bool {
    if subject == object || rule.edge_type != predicate {
        return false;
    }
    let label_is = |key: &str, label: &str| db.node_ref(key).is_some_and(|n| n.label() == label);
    label_is(subject, &rule.src_label)
        && label_is(object, &rule.dst_label)
        && predicate_holds(db, rule, subject, object)
}

/// Whether `rule`, which [`guard_matches`] for this pair, would in fact
/// derive `subject → object` from what the store holds now.
///
/// What the guard leaves out, as the rule engine applies it:
///
/// - **Namespace.** A scoped rule sees a node only in its own namespace, and
///   that goes for the via node as well as the two ends.
/// - **The via hop.** A via-hop rule evaluates its predicate between a via
///   node and the destination, not between the two ends: some node of
///   `via_label`, one `via_edge` hop from the source in `via_dir`, has to
///   satisfy it.
/// - **The cap.** A rule with `max_edges: Some(k)` keeps its best `k`
///   targets per source; without one, the rule stops at a global budget.
///   Ranking the candidates is the engine's business, so this answers yes
///   only when the cap cannot bind: no more candidates than the cap admits.
///   Past that the answer is no, which errs toward the weaker claim.
fn would_derive<F: Fs>(db: &GraphDb<F>, rule: &RuleDef, subject: &str, object: &str) -> bool {
    let sees = |key: &str| match rule.namespace.as_deref() {
        None => true,
        Some(ns) => namespace_of_value(db.get_prop(key, NS_PROP).as_ref()) == ns,
    };
    if !sees(subject) || !sees(object) {
        return false;
    }
    let sources = db.nodes_with_label(&rule.src_label).len() as u64;
    let targets = db.nodes_with_label(&rule.dst_label).len() as u64;
    let cap_cannot_bind = match rule.max_edges {
        Some(k) => targets <= k,
        None => sources.saturating_mul(targets) <= DEFAULT_MAX_EDGES,
    };
    if !cap_cannot_bind {
        return false;
    }
    let (Some(via_label), Some(via_edge)) = (rule.via_label.as_deref(), rule.via_edge.as_deref())
    else {
        // A plain rule: the guard already evaluated its predicate on the pair.
        return true;
    };
    db.neighbors(subject, via_edge, rule.via_dir.unwrap_or(Direction::Out))
        .unwrap_or_default()
        .iter()
        .any(|via| {
            db.node_ref(via).is_some_and(|n| n.label() == via_label)
                && sees(via)
                && predicate_holds(db, rule, via, object)
        })
}
