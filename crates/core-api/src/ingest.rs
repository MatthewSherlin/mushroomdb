use crate::db::GraphDb;
use core_rules::{Predicate, RuleDef};
use core_storage::fs::Fs;
use core_storage::{Result, Value};
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

/// Outcome of one [`GraphDb::ingest`] call. Row-level issues are collected here;
/// a commit-level `Err` means nothing was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestReport {
    pub inserted: usize,
    pub row_errors: Vec<(usize, String)>,
    pub rules_created: Vec<String>,
    pub skipped_fk_fields: Vec<(String, String)>,
}

type PropMap = BTreeMap<String, Value>;

struct Classified {
    accepted: Vec<(String, PropMap)>,
    row_errors: Vec<(usize, String)>,
}

/// Classify rows, optionally infer auto-FK rules, and commit one atomic batch
/// (rules first, then node inserts).
pub(crate) fn run<F: Fs>(
    db: &mut GraphDb<F>,
    label: &str,
    rows: Vec<BTreeMap<String, Value>>,
    opts: &IngestOptions,
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
    batch.commit()?;

    Ok(IngestReport {
        inserted: accepted.len(),
        row_errors,
        rules_created,
        skipped_fk_fields,
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
) -> (Vec<RuleDef>, Vec<(String, String)>) {
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
            0 => skipped.push((field, "no matching target keys".into())),
            1 => {
                let dst_label = labels.into_iter().next().expect("len == 1");
                let name = format!("auto_fk_{field}");
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
                });
            }
            _ => {
                let listed = labels.into_iter().collect::<Vec<_>>().join(", ");
                skipped.push((field, format!("ambiguous target labels: {listed}")));
            }
        }
    }

    (new_rules, skipped)
}
