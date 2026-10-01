//! What a store holds and how it is wired: the answer to "what's in here?".
//!
//! The session brief ([`crate::memory::brief`]) already knows a store's labels
//! and edge types, but it is shaped for a hook that runs once per session: it
//! omits the rules' predicates, every index declaration and the provisional
//! nodes `remember` stubbed. An assistant on a host with no `SessionStart`
//! hook has no brief at all. This report is the brief's counts plus those
//! three, for the MCP `schema` tool.
//!
//! [`render_schema`] does **not** stamp the untrusted-data framing line. The
//! MCP layer's `ok()` is the one place a task reply is framed; a renderer that
//! framed too would put the line in a reply twice.

use crate::digest::{sanitize, SEP};
use crate::explain_digest::predicate_summary;
use crate::memory::brief::{brief, BriefOptions, SchemaBrief};
use crate::memory_schema::{PROVISIONAL_LABEL, PROVISIONAL_PROP};
use crate::{CmpOp, Filter, GraphDb, PredicateSummary, Value};
use core_storage::fs::Fs;
use serde::Serialize;

/// Entries listed per section before the rest are counted instead.
pub const SCHEMA_LIST_CAP: usize = 20;

/// Provisional keys named by the report; the rest are counted.
pub const PROVISIONAL_SAMPLE: usize = 10;

/// One rule, as the schema names it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuleBrief {
    pub name: String,
    pub src_label: String,
    pub dst_label: String,
    pub edge_type: String,
    /// The predicate in one clause: what it compares, on which fields, and
    /// the threshold.
    pub predicate: String,
    /// `None` for a global rule, which links across namespaces.
    pub namespace: Option<String>,
}

/// Everything [`render_schema`] prints, for `json: true`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SchemaReport {
    pub brief: SchemaBrief,
    pub rules: Vec<RuleBrief>,
    /// `(label, field)` pairs `recall` searches.
    pub fulltext: Vec<(String, String)>,
    /// `(label, field)` pairs with an equality index.
    pub indexes: Vec<(String, String)>,
    /// How many nodes are provisional — named by `remember` before anything
    /// described them. They never expire (spec O-4); `forget` removes one.
    pub provisional: usize,
    /// The first [`PROVISIONAL_SAMPLE`] of them, in key order.
    pub provisional_sample: Vec<String>,
}

/// Every provisional node's key, sorted.
///
/// An `Entity` is not necessarily provisional — `upsert_entity` can create one
/// deliberately — so the mark is read, not the label alone.
pub fn provisional_keys<F: Fs>(db: &GraphDb<F>) -> Vec<String> {
    let filter = Filter::Cmp {
        field: PROVISIONAL_PROP.to_string(),
        op: CmpOp::Eq,
        value: Value::Bool(true),
    };
    let mut keys: Vec<String> = db
        .find_nodes(PROVISIONAL_LABEL, &filter)
        .into_iter()
        .map(|n| n.key().to_string())
        .collect();
    keys.sort();
    keys
}

/// The report for `db`. `opts` bounds the brief's part of the work.
pub fn schema_report<F: Fs>(db: &GraphDb<F>, opts: &BriefOptions) -> SchemaReport {
    let mut rules: Vec<RuleBrief> = db
        .rules()
        .into_iter()
        .map(|r| RuleBrief {
            predicate: predicate_summary(&PredicateSummary::from(&r.predicate)),
            name: r.name,
            src_label: r.src_label,
            dst_label: r.dst_label,
            edge_type: r.edge_type,
            namespace: r.namespace,
        })
        .collect();
    rules.sort_by(|a, b| a.name.cmp(&b.name));
    let provisional = provisional_keys(db);
    SchemaReport {
        brief: brief(db, opts),
        rules,
        fulltext: db.fulltext_pairs(),
        indexes: db.index_pairs(),
        provisional: provisional.len(),
        provisional_sample: provisional.into_iter().take(PROVISIONAL_SAMPLE).collect(),
    }
}

/// `items` one per line under `heading`, cut at [`SCHEMA_LIST_CAP`] with the
/// rest counted. Nothing at all when `items` is empty.
fn section(out: &mut String, heading: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str(heading);
    out.push_str(":\n");
    for item in items.iter().take(SCHEMA_LIST_CAP) {
        out.push_str("  ");
        out.push_str(item);
        out.push('\n');
    }
    if items.len() > SCHEMA_LIST_CAP {
        out.push_str(&format!("  … and {} more\n", items.len() - SCHEMA_LIST_CAP));
    }
}

fn pairs(ps: &[(String, String)]) -> Vec<String> {
    ps.iter()
        .map(|(l, f)| format!("{}.{}", sanitize(l), sanitize(f)))
        .collect()
}

/// The report as text, every value sanitized. Unframed: see the module docs.
#[must_use]
pub fn render_schema(r: &SchemaReport) -> String {
    let b = &r.brief;
    let at_least = if b.partial { "≥ " } else { "" };
    let mut out = format!(
        "mushroomdb schema — {at_least}{} node(s){SEP}{} edge(s){SEP}{} label(s)\n",
        b.nodes,
        b.edges,
        b.labels.len(),
    );
    let labels: Vec<String> = b
        .labels
        .iter()
        .map(|l| {
            let props: Vec<String> = l.props.iter().map(|p| sanitize(p)).collect();
            let hidden = if l.hidden_props > 0 {
                format!(" (+{} more)", l.hidden_props)
            } else {
                String::new()
            };
            format!(
                "{} ({}): {}{hidden}",
                sanitize(&l.label),
                l.nodes,
                props.join(", ")
            )
        })
        .collect();
    section(&mut out, "labels", &labels);
    let edge_types: Vec<String> = b
        .edge_types
        .iter()
        .map(|e| {
            let ends = |v: &[String]| v.iter().map(|s| sanitize(s)).collect::<Vec<_>>().join(", ");
            let rule = match &e.rule {
                Some(name) if e.hidden_rules > 0 => {
                    format!(" — rule {} (+{} more)", sanitize(name), e.hidden_rules)
                }
                Some(name) => format!(" — rule {}", sanitize(name)),
                None => String::new(),
            };
            format!(
                "{} ({}) {} → {}{rule}",
                sanitize(&e.edge_type),
                e.edges,
                ends(&e.src),
                ends(&e.dst)
            )
        })
        .collect();
    section(&mut out, "edge types", &edge_types);
    let rules: Vec<String> = r
        .rules
        .iter()
        .map(|rule| {
            let scope = match &rule.namespace {
                Some(ns) => format!("namespace {}", sanitize(ns)),
                None => "global".to_string(),
            };
            format!(
                "{}: {} → {} derives {} — {} ({scope})",
                sanitize(&rule.name),
                sanitize(&rule.src_label),
                sanitize(&rule.dst_label),
                sanitize(&rule.edge_type),
                rule.predicate
            )
        })
        .collect();
    section(&mut out, "rules", &rules);
    section(
        &mut out,
        "full-text (recall searches these)",
        &pairs(&r.fulltext),
    );
    section(&mut out, "equality indexes", &pairs(&r.indexes));
    if r.provisional > 0 {
        let named: Vec<String> = r.provisional_sample.iter().map(|k| sanitize(k)).collect();
        let more = r.provisional.saturating_sub(named.len());
        out.push_str(&format!(
            "provisional: {} — named but not yet described: {}{}\n",
            r.provisional,
            named.join(", "),
            if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            }
        ));
    }
    if b.partial {
        out.push_str("(partial: the time budget ran out; counts are lower bounds)\n");
    }
    out
}
