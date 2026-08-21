use crate::db::GraphDb;
use core_rules::{Predicate, RuleDef};
use core_storage::fs::Fs;
use core_storage::{GraphError, Result, Value};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Options for [`GraphDb::ingest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOptions {
    /// Property used as the node key. Also stored as a normal property.
    pub key_field: String,
    pub auto_fk: AutoFk,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            key_field: "id".into(),
            auto_fk: AutoFk::default(),
        }
    }
}

/// Zero-config FK inference: declare a `KeyMatch` rule per `*_id` field, or skip.
///
/// Auto-declared rule names are `auto_fk_<src_label_lowercase>_<field>`
/// (e.g. `auto_fk_person_org_id`, `auto_fk_device_org_id`) so two ingested
/// labels sharing an FK field each get their own rule. A name collision is
/// only the same `(label, field)` pair, where silent skip is correct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoFk {
    Auto { suffix: String },
    Off,
}

impl Default for AutoFk {
    fn default() -> Self {
        AutoFk::Auto {
            suffix: "_id".into(),
        }
    }
}

/// One auto-FK field that was not turned into a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FkSkip {
    pub field: String,
    pub reason: String,
}

/// Outcome of one [`GraphDb::ingest`] call. Row-level issues are collected here;
/// a commit-level `Err` means nothing was applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IngestReport {
    pub inserted: usize,
    pub row_errors: Vec<(usize, String)>,
    pub rules_created: Vec<String>,
    pub skipped_fk_fields: Vec<FkSkip>,
    /// User edges from the same request that were newly inserted (duplicates
    /// are no-ops and do not count).
    pub edges_inserted: usize,
}

/// Convert a JSON value to a stored [`Value`].
///
/// JSON `null` returns `None` so the caller can skip the field (not an error).
/// Integral numbers become [`Value::Int`]; other numbers become [`Value::Float`].
/// Arrays of scalars become [`Value::List`]. Nested objects and arrays that are
/// not arrays of scalars also return `None`; [`GraphDb::ingest_json`] treats
/// those as a per-row error rather than a skipped field.
pub fn json_to_value(v: serde_json::Value) -> Option<Value> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(Value::Bool(b)),
        serde_json::Value::Number(n) => number_to_value(&n),
        serde_json::Value::String(s) => Some(Value::Str(s)),
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(json_to_value(item)?);
            }
            Some(Value::List(out))
        }
        serde_json::Value::Object(_) => None,
    }
}

fn number_to_value(n: &serde_json::Number) -> Option<Value> {
    if let Some(i) = n.as_i64() {
        return Some(Value::Int(i));
    }
    let f = n.as_f64()?;
    if f.is_finite() && f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        Some(Value::Int(f as i64))
    } else {
        Some(Value::Float(f))
    }
}

fn is_json_scalar(v: &serde_json::Value) -> bool {
    matches!(
        v,
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) | serde_json::Value::String(_)
    )
}

fn field_shape_error(field: &str, v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Object(_) => Some(format!("nested object in field {field}")),
        serde_json::Value::Array(items) => {
            if items.iter().all(is_json_scalar) {
                None
            } else if items.iter().any(|x| x.is_object()) {
                Some(format!("array of objects in field {field}"))
            } else {
                Some(format!("mixed or null element in array field {field}"))
            }
        }
        _ => None,
    }
}

fn object_to_row(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> std::result::Result<BTreeMap<String, Value>, String> {
    let mut row = BTreeMap::new();
    for (k, v) in obj {
        if let Some(err) = field_shape_error(k, v) {
            return Err(err);
        }
        if let Some(val) = json_to_value(v.clone()) {
            row.insert(k.clone(), val);
        }
    }
    Ok(row)
}

/// Parsed JSON rows ready for [`crate::GraphDb::ingest`], plus bookkeeping so
/// per-row shape errors keep their original JSON-array indices.
pub struct JsonRows {
    /// Rows that passed shape checks, in original order.
    pub rows: Vec<BTreeMap<String, Value>>,
    kept_indices: Vec<usize>,
    shape_errors: Vec<(usize, String)>,
}

impl JsonRows {
    /// Remap ingest row-error indices onto the original JSON array and append
    /// the shape errors collected by [`json_to_rows`].
    pub fn into_report(self, mut report: IngestReport) -> IngestReport {
        for (idx, _) in &mut report.row_errors {
            *idx = self.kept_indices[*idx];
        }
        report.row_errors.extend(self.shape_errors);
        report.row_errors.sort_by_key(|(i, _)| *i);
        report
    }
}

/// Convert a parsed JSON value (must be an array of objects) into ingest rows.
///
/// Same conversion as [`crate::GraphDb::ingest_json`]: [`json_to_value`] per
/// field; nested objects / mixed arrays become per-row errors. A top-level
/// value that is not an array of objects is [`GraphError::IngestError`].
pub fn json_to_rows(value: &serde_json::Value) -> Result<JsonRows> {
    let arr = value.as_array().ok_or_else(|| GraphError::IngestError {
        detail: "top-level JSON must be an array of objects".into(),
    })?;
    if !arr.iter().all(|v| v.is_object()) {
        return Err(GraphError::IngestError {
            detail: "top-level JSON must be an array of objects".into(),
        });
    }

    let mut rows = Vec::new();
    let mut shape_errors = Vec::new();
    let mut kept_indices = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .expect("top-level checked as array of objects");
        match object_to_row(obj) {
            Ok(row) => {
                kept_indices.push(i);
                rows.push(row);
            }
            Err(msg) => shape_errors.push((i, msg)),
        }
    }
    Ok(JsonRows {
        rows,
        kept_indices,
        shape_errors,
    })
}

/// Parse JSON, convert rows, then delegate to [`run`].
pub(crate) fn run_json<F: Fs>(
    db: &mut GraphDb<F>,
    label: &str,
    json: &str,
    opts: &IngestOptions,
) -> Result<IngestReport> {
    let parsed: serde_json::Value =
        serde_json::from_str(json).map_err(|e| GraphError::IngestError {
            detail: e.to_string(),
        })?;
    let mut converted = json_to_rows(&parsed)?;
    let rows = std::mem::take(&mut converted.rows);
    let report = run(db, label, rows, opts, &[])?;
    Ok(converted.into_report(report))
}

type PropMap = BTreeMap<String, Value>;

struct Classified {
    accepted: Vec<(String, PropMap)>,
    row_errors: Vec<(usize, String)>,
}

/// Classify rows, optionally infer auto-FK rules, and commit one atomic batch
/// (rules first, then node inserts, then optional user edges).
pub(crate) fn run<F: Fs>(
    db: &mut GraphDb<F>,
    label: &str,
    rows: Vec<BTreeMap<String, Value>>,
    opts: &IngestOptions,
    edges: &[(String, String, String)],
) -> Result<IngestReport> {
    let Classified {
        accepted,
        row_errors,
    } = classify_rows(db, rows, &opts.key_field);

    let (new_rules, skipped_fk_fields) = match &opts.auto_fk {
        AutoFk::Off => (Vec::new(), Vec::new()),
        AutoFk::Auto { suffix } => infer_auto_fk(db, label, suffix, &opts.key_field, &accepted),
    };

    let rules_created: Vec<String> = new_rules.iter().map(|r| r.name.clone()).collect();

    let mut batch = db.batch();
    for def in new_rules {
        batch.create_rule(def);
    }
    for (key, props) in &accepted {
        let prop_vec: Vec<(String, Value)> =
            props.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        batch.insert_node(label, key, prop_vec);
    }
    for (etype, src, dst) in edges {
        batch.insert_edge(etype, src, dst);
    }
    let (_, edges_inserted) = batch.commit_ingest(label, accepted.len())?;

    Ok(IngestReport {
        inserted: accepted.len(),
        row_errors,
        rules_created,
        skipped_fk_fields,
        edges_inserted,
    })
}

fn classify_rows<F: Fs>(db: &GraphDb<F>, rows: Vec<PropMap>, key_field: &str) -> Classified {
    let mut accepted = Vec::new();
    let mut row_errors = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for (i, row) in rows.into_iter().enumerate() {
        match row.get(key_field) {
            None => row_errors.push((i, format!("missing key field {key_field}"))),
            Some(Value::Str(key)) => {
                if db.has_node(key) || seen.contains(key) {
                    row_errors.push((i, format!("duplicate key {key}")));
                } else {
                    seen.insert(key.clone());
                    accepted.push((key.clone(), row));
                }
            }
            Some(_) => row_errors.push((i, format!("key field {key_field} is not a string"))),
        }
    }
    Classified {
        accepted,
        row_errors,
    }
}

fn infer_auto_fk<F: Fs>(
    db: &GraphDb<F>,
    src_label: &str,
    suffix: &str,
    key_field: &str,
    accepted: &[(String, PropMap)],
) -> (Vec<RuleDef>, Vec<FkSkip>) {
    let existing_rule_names: BTreeSet<String> = db.rules().into_iter().map(|r| r.name).collect();
    let accepted_keys: BTreeSet<&str> = accepted.iter().map(|(k, _)| k.as_str()).collect();

    let mut fields: BTreeSet<String> = BTreeSet::new();
    for (_, row) in accepted {
        for field in row.keys() {
            if field != key_field && field.ends_with(suffix) && field.len() > suffix.len() {
                fields.insert(field.clone());
            }
        }
    }

    let mut new_rules = Vec::new();
    let mut skipped = Vec::new();

    for field in fields {
        let mut values: BTreeSet<&str> = BTreeSet::new();
        for (_, row) in accepted {
            if let Some(Value::Str(s)) = row.get(&field) {
                values.insert(s.as_str());
            }
        }

        let mut labels: BTreeSet<String> = BTreeSet::new();
        for value in values {
            if let Some(n) = db.node_ref(value) {
                labels.insert(n.label().to_string());
            }
            if accepted_keys.contains(value) {
                labels.insert(src_label.to_string());
            }
        }

        match labels.len() {
            0 => skipped.push(FkSkip {
                field,
                reason: "no matching target keys".into(),
            }),
            1 => {
                let dst_label = labels.into_iter().next().expect("len == 1");
                // `auto_fk_<src_label_lowercase>_<field>` — scoped by source
                // label so Person.org_id and Device.org_id do not collide.
                let name = format!("auto_fk_{}_{field}", src_label.to_lowercase());
                if existing_rule_names.contains(&name) {
                    continue;
                }
                let remainder = &field[..field.len() - suffix.len()];
                new_rules.push(RuleDef {
                    name,
                    src_label: src_label.to_string(),
                    dst_label,
                    predicate: Predicate::KeyMatch {
                        field: field.clone(),
                    },
                    edge_type: remainder.to_uppercase(),
                    weight_prop: None,
                    max_edges: None,
                    approximate: false,
                });
            }
            _ => {
                let listed = labels.into_iter().collect::<Vec<_>>().join(", ");
                skipped.push(FkSkip {
                    field,
                    reason: format!("ambiguous target labels: {listed}"),
                });
            }
        }
    }

    (new_rules, skipped)
}
