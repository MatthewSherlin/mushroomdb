//! `EXPLAIN` (QL-11): a statement's plan as one JSON string, without running it.
//!
//! `EXPLAIN` is a read. Every read entry point — the handle, `query_write`, a
//! masked or role-scoped read, the lock-free reader, a transaction, an as-of
//! handle — answers it the way it answers `CALL`: it hands [`explain_answer`]
//! the tokens and the schema listing it reads, and gets back a one-row result
//! with the column `plan`. The statement after `EXPLAIN` is never executed, so
//! `EXPLAIN CREATE …` writes nothing and a write statement's plan is shown
//! through a read-only entry point too.
//!
//! The plan names an index only when the caller could see that index in
//! `SHOW INDEXES`: under a mask, a role or a namespace, an index on a label
//! none of the caller's nodes carries is shown as `null`, as a scan is.

use super::ddl::SchemaListing;
use super::*;
use core_query::cypher::{
    explain_prefix, explain_statement, ExplainPrefix, IndexNames, Tok, PROFILE_REFUSAL,
};
use core_query::VisibleSet;

/// The declared indexes a caller may see, as [`IndexNames`].
struct SeenIndexes<'a> {
    listing: &'a SchemaListing,
    /// The labels the caller can see; `None` is every label.
    labels: Option<HashSet<&'a str>>,
}

impl SeenIndexes<'_> {
    fn allows(&self, label: &str) -> bool {
        self.labels.as_ref().is_none_or(|l| l.contains(label))
    }
}

impl IndexNames for SeenIndexes<'_> {
    fn equality(&self, label: &str, field: &str) -> Option<String> {
        if !self.allows(label) {
            return None;
        }
        // The executor answers an equality lookup from an equality or a range
        // index on the pair, so either names it; equality first.
        self.listing
            .index_named_for(label, field, true)
            .or_else(|| self.listing.index_named_for(label, field, false))
    }

    fn range(&self, label: &str, field: &str) -> Option<String> {
        if !self.allows(label) {
            return None;
        }
        self.listing.index_named_for(label, field, false)
    }
}

/// The caller's view of the world: all of it, or the nodes `visible` of
/// `view`.
pub(crate) enum Seen<'v, 'a> {
    All,
    Masked(&'v GraphView<'a>, &'v VisibleSet),
}

/// The answer to `tokens` when they are an `EXPLAIN` statement; `None` when
/// they are not. `PROFILE` is refused by name. A statement that does not parse
/// or plan is an error, prefixed as every entry point prefixes them.
pub(crate) fn explain_answer(
    tokens: &[Tok],
    listing: &SchemaListing,
    seen: Seen<'_, '_>,
) -> Result<Option<ResultSet>> {
    let rest = match explain_prefix(tokens) {
        ExplainPrefix::None => return Ok(None),
        ExplainPrefix::Profile => {
            return Err(GraphError::QueryError {
                detail: format!("parse: {PROFILE_REFUSAL}"),
            })
        }
        ExplainPrefix::Explain(rest) => rest,
    };
    let labels = match seen {
        Seen::All => None,
        Seen::Masked(view, visible) => Some(listing.visible_labels(view, visible)),
    };
    let indexes = SeenIndexes { listing, labels };
    let plan =
        explain_statement(rest, &indexes).map_err(|detail| GraphError::QueryError { detail })?;
    let mut rs = ResultSet::new(vec!["plan".to_string()]);
    rs.push_row(vec![Some(Value::Str(plan))]);
    Ok(Some(rs))
}

/// Whether `cypher` is an `EXPLAIN` statement. `Err` only when it fails to
/// lex. A protocol that reports a plan in its own summary (Bolt) asks this to
/// know a result is one.
pub fn is_explain_query(cypher: &str) -> std::result::Result<bool, String> {
    let tokens = core_query::cypher::lex(cypher).map_err(|e| format!("lex: {e}"))?;
    Ok(matches!(explain_prefix(&tokens), ExplainPrefix::Explain(_)))
}

/// The plan an `EXPLAIN` result carries, as the nested map a Bolt summary's
/// `plan` holds: `operatorType`, `args`, `identifiers` and `children`, with the
/// last operator of the pipeline at the root and the one before it as its
/// child. `None` when `rs` is not an `EXPLAIN` result.
pub fn explain_plan_value(rs: &ResultSet) -> Option<Value> {
    let Some(Value::Str(json)) = (rs.columns() == ["plan"] && rs.len() == 1)
        .then(|| rs.row(0).first().cloned().flatten())
        .flatten()
    else {
        return None;
    };
    let parsed: serde_json::Value = serde_json::from_str(&json).ok()?;
    plan_tree(parsed.as_array()?)
}

fn json_to_value(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        serde_json::Value::Object(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

/// The fields of a plan operator that name a variable it binds or reads.
const IDENTIFIER_FIELDS: [&str; 6] = ["var", "from", "to", "rel_var", "alias", "path_var"];

fn plan_tree(ops: &[serde_json::Value]) -> Option<Value> {
    // A UNION is a list of parts, not a pipeline.
    if ops.iter().all(|o| o["op"] == "Union") && !ops.is_empty() {
        let children = ops
            .iter()
            .filter_map(|o| plan_tree(o["plan"].as_array()?))
            .collect();
        return Some(node("Union", BTreeMap::new(), Vec::new(), children));
    }
    let mut previous: Option<Value> = None;
    for op in ops {
        let fields = op.as_object()?;
        let name = fields.get("op")?.as_str()?;
        let mut args = BTreeMap::new();
        let mut identifiers: Vec<String> = Vec::new();
        let mut children: Vec<Value> = previous.take().into_iter().collect();
        for (key, value) in fields {
            if key == "op" {
                continue;
            }
            if key == "inner" {
                if let Some(inner) = value.as_array().and_then(|a| plan_tree(a)) {
                    children.push(inner);
                }
                continue;
            }
            if IDENTIFIER_FIELDS.contains(&key.as_str()) {
                if let Some(id) = value.as_str() {
                    if !identifiers.iter().any(|i| i == id) {
                        identifiers.push(id.to_string());
                    }
                }
            }
            args.insert(key.clone(), json_to_value(value));
        }
        previous = Some(node(name, args, identifiers, children));
    }
    previous
}

fn node(
    operator: &str,
    args: BTreeMap<String, Value>,
    identifiers: Vec<String>,
    children: Vec<Value>,
) -> Value {
    Value::Map(BTreeMap::from([
        ("operatorType".to_string(), Value::Str(operator.to_string())),
        ("args".to_string(), Value::Map(args)),
        (
            "identifiers".to_string(),
            Value::List(identifiers.into_iter().map(Value::Str).collect()),
        ),
        ("children".to_string(), Value::List(children)),
    ]))
}
