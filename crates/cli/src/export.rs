//! Export writer for `mushroomdb export`.
//!
//! Supports two formats:
//!   - `jsonl`: nodes.jsonl, edges.jsonl, rules.jsonl (always deterministic)
//!   - `parquet`: nodes.parquet, edges.parquet, rules.parquet (byte-identical
//!     output is NOT guaranteed across parquet-rs versions; use JSONL when
//!     byte-stable reproducibility is required)
//!
//! Two runs on the same store state produce byte-identical JSONL output (sorted
//! by key / (edge_type, src, dst) / name — callers must pre-sort the slices).
//!
//! # Float precision loss
//!
//! `Value::Float` values that are NaN or ±Inf are not representable in JSON.
//! They are serialised as JSON `null` rather than causing the export to fail.
//! This is a lossy mapping; the original value is irrecoverable from the
//! export.  Stores that need exact float fidelity should use the binary backup
//! command instead of export.

use core_api::{ExportEdge, NodeInfo, RuleDef, Value};
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use arrow_array::{ArrayRef, BooleanArray, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

/// Accepted export formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportFormat {
    Jsonl,
    Parquet,
}

impl ExportFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jsonl" => Some(Self::Jsonl),
            "parquet" => Some(Self::Parquet),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            Self::Parquet => "parquet",
        }
    }
}

/// Serialize a `Value` to serde_json `Value`.
///
/// NaN and ±Inf floats are not representable in JSON and are mapped to `null`
/// (see module-level docs for the loss semantics).
fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        // serde_json::Number::from_f64 returns None for NaN/±Inf; map those to null.
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::List(xs) => serde_json::Value::Array(xs.iter().map(value_to_json).collect()),
        Value::Map(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
    }
}

/// Write JSONL files to `dest`: nodes.jsonl, edges.jsonl, rules.jsonl.
///
/// `nodes` must be sorted by key, `edges` by (edge_type, src, dst), `rules` by name.
/// Deterministic: same sorted input → byte-identical output.
pub fn write_jsonl(
    nodes: &[NodeInfo],
    edges: &[ExportEdge],
    rules: &[RuleDef],
    dest: &Path,
) -> Result<(), crate::CliError> {
    std::fs::create_dir_all(dest)?;

    // ── nodes.jsonl ────────────────────────────────────────────────────────
    {
        let mut f = std::fs::File::create(dest.join("nodes.jsonl"))?;
        for node in nodes {
            let mut obj = serde_json::Map::new();
            obj.insert("key".into(), serde_json::Value::String(node.key.clone()));
            obj.insert(
                "label".into(),
                serde_json::Value::String(node.label.clone()),
            );
            for (k, v) in &node.props {
                obj.insert(k.clone(), value_to_json(v));
            }
            let line = serde_json::to_string(&serde_json::Value::Object(obj))
                .map_err(|e| crate::CliError(format!("json encode error: {e}")))?;
            writeln!(f, "{line}")?;
        }
    }

    // ── edges.jsonl ────────────────────────────────────────────────────────
    {
        let mut f = std::fs::File::create(dest.join("edges.jsonl"))?;
        for edge in edges {
            let obj = serde_json::json!({
                "edge_type": edge.edge_type,
                "src": edge.src,
                "dst": edge.dst,
                "derived": edge.derived,
                "rule": edge.rule,
            });
            let line = serde_json::to_string(&obj)
                .map_err(|e| crate::CliError(format!("json encode error: {e}")))?;
            writeln!(f, "{line}")?;
        }
    }

    // ── rules.jsonl ────────────────────────────────────────────────────────
    {
        let mut f = std::fs::File::create(dest.join("rules.jsonl"))?;
        for rule in rules {
            let line = serde_json::to_string(rule)
                .map_err(|e| crate::CliError(format!("json encode rule: {e}")))?;
            writeln!(f, "{line}")?;
        }
    }

    Ok(())
}

/// Write parquet files to `dest`: nodes.parquet, edges.parquet, rules.parquet.
///
/// Schema for each file:
/// - nodes.parquet: key (Utf8), label (Utf8), props (Utf8, JSON)
/// - edges.parquet: edge_type (Utf8), src (Utf8), dst (Utf8), derived (Boolean), rule (Utf8, nullable)
/// - rules.parquet: name (Utf8), definition (Utf8, JSON)
///
/// `nodes` must be sorted by key, `edges` by (edge_type, src, dst), `rules` by name.
///
/// Compression: Snappy (parquet-rs default).  This is a format detail, not a
/// stability contract — the column encoding and page layout may differ across
/// parquet-rs versions.  Use JSONL export when byte-stable reproducibility is
/// required.
pub fn write_parquet(
    nodes: &[NodeInfo],
    edges: &[ExportEdge],
    rules: &[RuleDef],
    dest: &Path,
) -> Result<(), crate::CliError> {
    std::fs::create_dir_all(dest)?;
    let props = WriterProperties::builder().build();

    // ── nodes.parquet ──────────────────────────────────────────────────────
    {
        let schema = Arc::new(Schema::new(vec![
            Field::new("key", DataType::Utf8, false),
            Field::new("label", DataType::Utf8, false),
            Field::new("props", DataType::Utf8, false),
        ]));
        let mut keys = Vec::new();
        let mut labels = Vec::new();
        let mut props_json = Vec::new();
        for node in nodes {
            keys.push(node.key.clone());
            labels.push(node.label.clone());
            let p: serde_json::Map<String, serde_json::Value> = node
                .props
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            props_json.push(
                serde_json::to_string(&serde_json::Value::Object(p))
                    .map_err(|e| crate::CliError(format!("json encode: {e}")))?,
            );
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(keys)) as ArrayRef,
                Arc::new(StringArray::from(labels)) as ArrayRef,
                Arc::new(StringArray::from(props_json)) as ArrayRef,
            ],
        )
        .map_err(|e| crate::CliError(format!("arrow error: {e}")))?;

        let file = std::fs::File::create(dest.join("nodes.parquet"))?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(props.clone()))
            .map_err(|e| crate::CliError(format!("parquet writer: {e}")))?;
        writer
            .write(&batch)
            .map_err(|e| crate::CliError(format!("parquet write: {e}")))?;
        writer
            .close()
            .map_err(|e| crate::CliError(format!("parquet close: {e}")))?;
    }

    // ── edges.parquet ──────────────────────────────────────────────────────
    {
        let schema = Arc::new(Schema::new(vec![
            Field::new("edge_type", DataType::Utf8, false),
            Field::new("src", DataType::Utf8, false),
            Field::new("dst", DataType::Utf8, false),
            Field::new("derived", DataType::Boolean, false),
            Field::new("rule", DataType::Utf8, true),
        ]));
        let mut etypes = Vec::new();
        let mut srcs = Vec::new();
        let mut dsts = Vec::new();
        let mut deriveds = Vec::new();
        let mut rule_names: Vec<Option<String>> = Vec::new();
        for edge in edges {
            etypes.push(edge.edge_type.clone());
            srcs.push(edge.src.clone());
            dsts.push(edge.dst.clone());
            deriveds.push(edge.derived);
            rule_names.push(edge.rule.clone());
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(etypes)) as ArrayRef,
                Arc::new(StringArray::from(srcs)) as ArrayRef,
                Arc::new(StringArray::from(dsts)) as ArrayRef,
                Arc::new(BooleanArray::from(deriveds)) as ArrayRef,
                Arc::new(StringArray::from(rule_names)) as ArrayRef,
            ],
        )
        .map_err(|e| crate::CliError(format!("arrow error: {e}")))?;

        let file = std::fs::File::create(dest.join("edges.parquet"))?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(props.clone()))
            .map_err(|e| crate::CliError(format!("parquet writer: {e}")))?;
        writer
            .write(&batch)
            .map_err(|e| crate::CliError(format!("parquet write: {e}")))?;
        writer
            .close()
            .map_err(|e| crate::CliError(format!("parquet close: {e}")))?;
    }

    // ── rules.parquet ──────────────────────────────────────────────────────
    {
        let schema = Arc::new(Schema::new(vec![
            Field::new("name", DataType::Utf8, false),
            Field::new("definition", DataType::Utf8, false),
        ]));
        let mut names = Vec::new();
        let mut definitions = Vec::new();
        for rule in rules {
            names.push(rule.name.clone());
            definitions.push(
                serde_json::to_string(rule)
                    .map_err(|e| crate::CliError(format!("json encode: {e}")))?,
            );
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(names)) as ArrayRef,
                Arc::new(StringArray::from(definitions)) as ArrayRef,
            ],
        )
        .map_err(|e| crate::CliError(format!("arrow error: {e}")))?;

        let file = std::fs::File::create(dest.join("rules.parquet"))?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(props))
            .map_err(|e| crate::CliError(format!("parquet writer: {e}")))?;
        writer
            .write(&batch)
            .map_err(|e| crate::CliError(format!("parquet write: {e}")))?;
        writer
            .close()
            .map_err(|e| crate::CliError(format!("parquet close: {e}")))?;
    }

    Ok(())
}
