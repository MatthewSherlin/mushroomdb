//! Export writer for `mushroomdb export`.
//!
//! Supports three formats:
//!   - `jsonl`: nodes.jsonl, edges.jsonl, rules.jsonl (always deterministic)
//!   - `parquet`: nodes.parquet, edges.parquet, rules.parquet (byte-identical
//!     output is NOT guaranteed across parquet-rs versions; use JSONL when
//!     byte-stable reproducibility is required)
//!   - `graphml`: a single `.graphml` file (nodes + edges only; rules have no
//!     GraphML analogue) for import into generic graph viewers and analysis
//!     tools
//!
//! Two runs on the same store state produce byte-identical JSONL and GraphML
//! output (sorted by key / (edge_type, src, dst) / name — callers must
//! pre-sort the slices).
//!
//! # Float precision loss
//!
//! `Value::Float` values that are NaN or ±Inf are not representable in JSON
//! or in GraphML's numeric attribute types. They are serialised as JSON
//! `null` (JSONL) or an empty `<data>` element (GraphML) rather than causing
//! the export to fail. This is a lossy mapping; the original value is
//! irrecoverable from the export. Stores that need exact float fidelity
//! should use the binary backup command instead of export.

use core_api::{ExportEdge, NodeInfo, RuleDef, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
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
    Graphml,
}

impl ExportFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jsonl" => Some(Self::Jsonl),
            "parquet" => Some(Self::Parquet),
            "graphml" => Some(Self::Graphml),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            Self::Parquet => "parquet",
            Self::Graphml => "graphml",
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

// ── GraphML ──────────────────────────────────────────────────────────────

/// GraphML `attr.type` values this writer produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum GmlType {
    Boolean,
    Int,
    Double,
    String,
}

impl GmlType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Int => "int",
            Self::Double => "double",
            Self::String => "string",
        }
    }
}

/// The GraphML attribute type for a scalar `Value`. Lists and maps are
/// JSON-encoded text, so they declare `string`.
fn value_gml_type(v: &Value) -> GmlType {
    match v {
        Value::Int(_) => GmlType::Int,
        Value::Float(_) => GmlType::Double,
        Value::Bool(_) => GmlType::Boolean,
        Value::Str(_) | Value::List(_) | Value::Map(_) => GmlType::String,
    }
}

/// Render a `Value` as GraphML `<data>` element text.
///
/// Reuses [`value_to_json`] so the NaN/±Inf → "no value" mapping is exactly
/// the same lossy conversion JSONL export makes (see module docs): such
/// floats become serde_json `Null`, which renders here as empty text.
/// Lists and maps render as their JSON text form, matching the `string`
/// `attr.type` declared for them.
fn value_gml_text(v: &Value) -> String {
    match value_to_json(v) {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

/// Escape `&`, `<`, `>`, `"`, `'` for safe use in GraphML element text and
/// attribute values, and strip characters the XML 1.0 grammar cannot encode
/// at all (control characters other than tab/LF/CR).
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' => {
                out.push(c)
            }
            _ => {} // strip: not a valid XML 1.0 character
        }
    }
    out
}

/// Sanitize one component of a GraphML key `id` to an XML NCName-safe form:
/// `[A-Za-z_]` for the first character, `[A-Za-z0-9_.-]` after. Any other
/// character becomes `_`.
fn sanitize_id_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        let ok = if i == 0 {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
        };
        out.push(if ok { c } else { '_' });
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Insert `base` into `used`, disambiguating with a numeric suffix on
/// collision (`base`, `base_2`, `base_3`, ...). Deterministic for a fixed
/// insertion order.
fn unique_id(base: String, used: &mut BTreeSet<String>) -> String {
    if used.insert(base.clone()) {
        return base;
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}_{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

/// Resolve the `.graphml` file to write for a `dest` argument.
///
/// If `dest` is an existing directory, the file is `dest/graph.graphml`.
/// Otherwise `dest` is treated as the file path itself and its parent
/// directories are created.
fn resolve_graphml_dest(dest: &Path) -> Result<PathBuf, crate::CliError> {
    if dest.is_dir() {
        Ok(dest.join("graph.graphml"))
    } else {
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        Ok(dest.to_path_buf())
    }
}

/// Write a single GraphML file describing `nodes` and `edges`.
///
/// Nodes carry `label` plus every property (lists/maps JSON-encoded as
/// `string`-typed text). Edges carry `type`, `derived`, and — when present —
/// `rule` and `weight`. `<key>` elements are declared once per (`for`, name)
/// pair, node keys before edge keys, each block sorted by attribute name.
/// Key `id`s are XML-name-safe (`n_<prop>` / `e_<prop>`, sanitized); the
/// original name is preserved in `attr.name`.
///
/// `nodes` must be sorted by key and `edges` by `(edge_type, src, dst)` —
/// same precondition as [`write_jsonl`]. Two runs on the same sorted input
/// produce byte-identical output: edge `id`s (`e0`, `e1`, ...) are assigned
/// in that sorted order.
///
/// Rules have no GraphML representation; only nodes and edges are exported.
/// See [`resolve_graphml_dest`] for how `dest` maps to the output file path.
pub fn write_graphml(
    nodes: &[NodeInfo],
    edges: &[ExportEdge],
    dest: &Path,
) -> Result<PathBuf, crate::CliError> {
    let file_path = resolve_graphml_dest(dest)?;

    // Node keys: `label` plus the union of every prop name across all nodes.
    // Type is taken from the first occurrence in (already sorted) node order.
    let mut node_key_types: BTreeMap<String, GmlType> = BTreeMap::new();
    node_key_types.insert("label".to_string(), GmlType::String);
    for node in nodes {
        for (k, v) in &node.props {
            node_key_types
                .entry(k.clone())
                .or_insert_with(|| value_gml_type(v));
        }
    }

    // Edge keys: a fixed schema, sorted by name.
    let mut edge_key_types: Vec<(&str, GmlType)> = vec![
        ("type", GmlType::String),
        ("derived", GmlType::Boolean),
        ("rule", GmlType::String),
        ("weight", GmlType::Double),
    ];
    edge_key_types.sort_by(|a, b| a.0.cmp(b.0));

    // Assign sanitized, collision-free key ids: node keys (`n_...`) first,
    // then edge keys (`e_...`), each block sorted by attribute name.
    let mut used_ids: BTreeSet<String> = BTreeSet::new();
    let mut node_key_ids: BTreeMap<String, String> = BTreeMap::new();
    for name in node_key_types.keys() {
        let base = format!("n_{}", sanitize_id_component(name));
        node_key_ids.insert(name.clone(), unique_id(base, &mut used_ids));
    }
    let mut edge_key_ids: BTreeMap<&str, String> = BTreeMap::new();
    for (name, _) in &edge_key_types {
        let base = format!("e_{}", sanitize_id_component(name));
        edge_key_ids.insert(name, unique_id(base, &mut used_ids));
    }

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">\n");

    for (name, gtype) in &node_key_types {
        let _ = writeln!(
            out,
            "  <key id=\"{}\" for=\"node\" attr.name=\"{}\" attr.type=\"{}\"/>",
            xml_escape(&node_key_ids[name]),
            xml_escape(name),
            gtype.as_str()
        );
    }
    for (name, gtype) in &edge_key_types {
        let _ = writeln!(
            out,
            "  <key id=\"{}\" for=\"edge\" attr.name=\"{}\" attr.type=\"{}\"/>",
            xml_escape(&edge_key_ids[name]),
            xml_escape(name),
            gtype.as_str()
        );
    }

    out.push_str("  <graph id=\"G\" edgedefault=\"directed\">\n");

    for node in nodes {
        let _ = writeln!(out, "    <node id=\"{}\">", xml_escape(&node.key));
        let _ = writeln!(
            out,
            "      <data key=\"{}\">{}</data>",
            xml_escape(&node_key_ids["label"]),
            xml_escape(&node.label)
        );
        for (k, v) in &node.props {
            let _ = writeln!(
                out,
                "      <data key=\"{}\">{}</data>",
                xml_escape(&node_key_ids[k]),
                xml_escape(&value_gml_text(v))
            );
        }
        out.push_str("    </node>\n");
    }

    for (i, edge) in edges.iter().enumerate() {
        let _ = writeln!(
            out,
            "    <edge id=\"e{i}\" source=\"{}\" target=\"{}\">",
            xml_escape(&edge.src),
            xml_escape(&edge.dst)
        );
        let _ = writeln!(
            out,
            "      <data key=\"{}\">{}</data>",
            xml_escape(&edge_key_ids["type"]),
            xml_escape(&edge.edge_type)
        );
        let _ = writeln!(
            out,
            "      <data key=\"{}\">{}</data>",
            xml_escape(&edge_key_ids["derived"]),
            edge.derived
        );
        if let Some(rule) = &edge.rule {
            let _ = writeln!(
                out,
                "      <data key=\"{}\">{}</data>",
                xml_escape(&edge_key_ids["rule"]),
                xml_escape(rule)
            );
        }
        if let Some(w) = edge.weight {
            let _ = writeln!(
                out,
                "      <data key=\"{}\">{}</data>",
                xml_escape(&edge_key_ids["weight"]),
                xml_escape(&value_gml_text(&Value::Float(w)))
            );
        }
        out.push_str("    </edge>\n");
    }

    out.push_str("  </graph>\n");
    out.push_str("</graphml>\n");

    std::fs::write(&file_path, out)?;
    Ok(file_path)
}
