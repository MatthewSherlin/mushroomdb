use crate::db::GraphDb;
use core_rules::{default_max_edges, Predicate, RuleDef};
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
/// JSON `null` returns `Ok(None)` so the caller can skip the field (not an
/// error). Integral numbers become [`Value::Int`]; other numbers become
/// [`Value::Float`]. Arrays become [`Value::List`] by recursing through this
/// function. If any element is JSON `null` the entire array field is `Ok(None)`
/// and the caller silently skips the field (same null-drop policy as top-level
/// fields). JSON objects become [`Value::Map`] recursively; `null` values
/// inside an object are silently omitted from the map.
///
/// **Behavior change from pre-Map:** JSON objects previously returned `None`
/// (treated as a skipped/error field by `ingest_json`). They now produce
/// `Value::Map` so nested objects are stored faithfully.
///
/// An object with exactly one key that is one of the five type tags is the
/// typed form of a format-V11 value: `{"$date": "2024-01-15"}`,
/// `{"$ldt": …}`, `{"$dt": …}`, `{"$dur": …}` and `{"$binary": "<base64>"}`
/// (see [`tagged_json`], the inverse). Its payload must be a string in the
/// tag's form; anything else is an `Err` naming the tag and the reason, never
/// a map — the caller adds the field name. A bare string is always
/// [`Value::Str`], never a date. Objects with other `$` keys, or with more
/// than one key, are maps as before.
pub fn json_to_value(v: serde_json::Value) -> std::result::Result<Option<Value>, String> {
    Ok(match v {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(Value::Bool(b)),
        serde_json::Value::Number(n) => number_to_value(&n),
        serde_json::Value::String(s) => Some(Value::Str(s)),
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.into_iter().enumerate() {
                match json_to_value(item).map_err(|e| format!("item {i}: {e}"))? {
                    Some(v) => out.push(v),
                    None => return Ok(None),
                }
            }
            Some(Value::List(out))
        }
        serde_json::Value::Object(obj) => {
            if let Some(typed) = tagged_object_value(&obj) {
                return typed.map(Some);
            }
            let mut map = std::collections::BTreeMap::new();
            for (k, v) in obj {
                let val = json_to_value(v).map_err(|e| format!("key {k:?}: {e}"))?;
                if let Some(val) = val {
                    map.insert(k, val);
                }
            }
            Some(Value::Map(map))
        }
    })
}

/// The typed value a single-key tagged object names, or `None` when the object
/// is not one (more than one key, or a key that is not one of the five tags).
fn tagged_object_value(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<std::result::Result<Value, String>> {
    if obj.len() != 1 {
        return None;
    }
    let (tag, payload) = obj.iter().next()?;
    if !core_storage::V11_TAGS.contains(&tag.as_str()) {
        return None;
    }
    Some(match payload.as_str() {
        Some(text) => core_storage::value_from_tagged_text(tag, text),
        None => Err(format!("{tag} payload must be a string, not {payload}")),
    })
}

/// The tagged JSON object of a format-V11 value — `Ok(Some({"$date":
/// "2024-01-15"}))` — or `Ok(None)` for the types JSON carries untagged.
///
/// The inverse of the tagged-object branch of [`json_to_value`]. Every JSON
/// encoder of `Value` calls this, so they all write the one form it reads.
/// `Err` is a value with no text (a `DateTime` offset outside ±18:00, see
/// [`core_storage::tagged_text`]); no tag is written for it.
pub fn tagged_json(v: &Value) -> std::result::Result<Option<serde_json::Value>, String> {
    let Some((tag, text)) = core_storage::tagged_text(v)? else {
        return Ok(None);
    };
    let mut obj = serde_json::Map::with_capacity(1);
    obj.insert(tag.to_string(), serde_json::Value::String(text));
    Ok(Some(serde_json::Value::Object(obj)))
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

fn object_to_row(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> std::result::Result<BTreeMap<String, Value>, String> {
    let mut row = BTreeMap::new();
    for (k, v) in obj {
        if let Some(val) = json_to_value(v.clone()).map_err(|e| format!("field {k:?}: {e}"))? {
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

    // One `WalRecord::Batch` (Batched fsync), not a loop of `insert_node`.
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
                // `id` mirrors the key (ledger row 128): a row naming another
                // `id` is refused here, per row, rather than failing the frame.
                if let Some(id) = row.get("id").filter(|id| *id != &Value::Str(key.clone())) {
                    row_errors.push((
                        i,
                        format!(
                            "id property {id:?} does not match the key {key:?}; `id` mirrors \
                             the key"
                        ),
                    ));
                } else if db.has_node(key) || seen.contains(key) {
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
                let predicate = Predicate::KeyMatch {
                    field: field.clone(),
                };
                let max_edges = Some(default_max_edges(&predicate));
                new_rules.push(RuleDef {
                    name,
                    src_label: src_label.to_string(),
                    dst_label,
                    predicate,
                    edge_type: remainder.to_uppercase(),
                    weight_prop: None,
                    max_edges,
                    approximate: false,
                    via_label: None,
                    via_edge: None,
                    via_dir: None,
                    namespace: None,
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

#[cfg(test)]
mod tests {
    use super::{json_to_rows, json_to_value, tagged_json};
    use core_storage::Value;
    use serde_json::json;

    #[test]
    fn test_value_json_roundtrip() {
        // Date: 2024-01-15 = days 19737 since 1970-01-01.
        let date = Value::Date(19_737);
        let j = tagged_json(&date).unwrap().unwrap();
        assert_eq!(j, json!({"$date": "2024-01-15"}));
        assert_eq!(json_to_value(j), Ok(Some(date)));
        // Binary: base64 roundtrip.
        let bin = Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        let j = tagged_json(&bin).unwrap().unwrap();
        assert_eq!(j, json!({"$binary": "3q2+7w=="}));
        assert_eq!(json_to_value(j), Ok(Some(bin)));
    }

    #[test]
    fn every_tagged_form_parses_to_its_type() {
        let cases = [
            (json!({"$date": "2024-01-15"}), Value::Date(19_737)),
            (
                json!({"$ldt": "2024-01-15T12:34:56.789012"}),
                Value::LocalDateTime(1_705_322_096_789_012),
            ),
            (
                json!({"$dt": "2024-01-15T12:34:56.789012+05:30"}),
                Value::DateTime {
                    micros_utc: 1_705_302_296_789_012,
                    offset_seconds: 19_800,
                },
            ),
            (
                json!({"$dt": "2024-01-15T12:00:00+14:00"}),
                Value::DateTime {
                    micros_utc: 1_705_269_600_000_000,
                    offset_seconds: 50_400,
                },
            ),
            (
                json!({"$dur": "P1Y2M3DT4H5M6.789S"}),
                Value::Duration {
                    months: 14,
                    days: 3,
                    nanos: 14_706_789_000_000,
                },
            ),
            (json!({"$binary": ""}), Value::Binary(vec![])),
        ];
        for (j, want) in cases {
            assert_eq!(json_to_value(j.clone()), Ok(Some(want.clone())), "{j}");
            assert_eq!(tagged_json(&want), Ok(Some(j)));
        }
        // Tagged values nest inside lists and maps like any other value.
        assert_eq!(
            json_to_value(json!([{"$date": "1970-01-01"}, {"k": {"$binary": "AA=="}}])),
            Ok(Some(Value::List(vec![
                Value::Date(0),
                Value::Map([("k".to_string(), Value::Binary(vec![0]))].into()),
            ])))
        );
    }

    #[test]
    fn a_bad_payload_under_a_known_tag_is_an_error_never_a_map() {
        let bad = [
            ("$date", json!("2024-1-5"), "an ISO 8601 date"),
            ("$date", json!("2023-02-29"), "an ISO 8601 date"),
            (
                "$ldt",
                json!("2024-01-15T12:00:00Z"),
                "an ISO 8601 local date-time",
            ),
            (
                "$dt",
                json!("2024-01-15T12:00:00"),
                "an ISO 8601 date-time with an offset",
            ),
            ("$dt", json!("2024-01-15T12:00:00+18:30"), "within ±18:00"),
            ("$dur", json!("P1D2M"), "an ISO 8601 duration"),
            ("$binary", json!("Zm9vYg"), "padded standard base64"),
        ];
        for (tag, payload, expects) in bad {
            let err = json_to_value(json!({ tag: payload.clone() })).unwrap_err();
            assert!(
                err.starts_with(&format!("{tag} payload {payload}")),
                "{err}"
            );
            assert!(err.contains(expects), "{err}");
        }
        // A payload that is not a string, under every tag.
        for tag in core_storage::V11_TAGS {
            for payload in [json!(19737), json!(null), json!(["x"]), json!({"a": 1})] {
                let err = json_to_value(json!({ tag: payload.clone() })).unwrap_err();
                assert_eq!(
                    err,
                    format!("{tag} payload must be a string, not {payload}")
                );
            }
        }
        // Nested, the error says where.
        let err = json_to_value(json!({"meta": [1, {"$date": "x"}]})).unwrap_err();
        assert!(
            err.starts_with(r#"key "meta": item 1: $date payload"#),
            "{err}"
        );
    }

    #[test]
    fn bare_strings_and_other_objects_keep_their_old_meaning() {
        // A bare ISO string is a string, never a date.
        assert_eq!(
            json_to_value(json!("2024-01-15")),
            Ok(Some(Value::Str("2024-01-15".into())))
        );
        // Not a tagged form: two keys, or a `$` key that is not one of the
        // five tags. Each stays the map it always was.
        for j in [
            json!({"$date": "2024-01-15", "x": 1}),
            json!({"$date": "not-a-date", "x": 1}),
            json!({"$time": "12:00"}),
            json!({"$ref": 7}),
        ] {
            assert!(
                matches!(json_to_value(j.clone()), Ok(Some(Value::Map(_)))),
                "{j}"
            );
        }
        // Untagged types have no tagged form.
        assert_eq!(tagged_json(&Value::Int(1)), Ok(None));
    }

    #[test]
    fn a_datetime_with_no_text_has_no_tag() {
        let bad = Value::DateTime {
            micros_utc: 0,
            offset_seconds: 64_801,
        };
        assert!(tagged_json(&bad).unwrap_err().contains("outside ±18:00"));
    }

    #[test]
    fn ingest_rows_report_a_bad_tag_as_a_row_error_naming_the_field() {
        let rows = json_to_rows(&json!([
            {"id": "a", "born": {"$date": "2024-1-5"}},
            {"id": "b"},
        ]))
        .unwrap();
        assert_eq!(rows.rows.len(), 1);
        let report = rows.into_report(super::IngestReport {
            inserted: 1,
            row_errors: vec![],
            rules_created: vec![],
            skipped_fk_fields: vec![],
            edges_inserted: 0,
        });
        assert_eq!(report.row_errors.len(), 1);
        let (index, msg) = &report.row_errors[0];
        assert_eq!(*index, 0);
        assert_eq!(
            msg,
            r#"field "born": $date payload "2024-1-5" is not an ISO 8601 date (YYYY-MM-DD)"#
        );
    }
}
