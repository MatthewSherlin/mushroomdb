//! Export writer for `mushroomdb export`.
//!
//! Supports five formats:
//!   - `jsonl`: nodes.jsonl, edges.jsonl, rules.jsonl (always deterministic)
//!   - `csv`: nodes.csv and relationships.csv, the files `mushroomdb import
//!     --nodes/--relationships` reads, with typed headers (`age:int`); plus
//!     rules.jsonl when the store has rules
//!   - `cypher`: graph.cypher, one statement per line, that this engine's own
//!     query language reads back
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

use mushroomdb::{ExportEdge, NodeInfo, RuleDef, Value};
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
    Csv,
    Cypher,
    Parquet,
    Graphml,
}

impl ExportFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jsonl" => Some(Self::Jsonl),
            "csv" => Some(Self::Csv),
            "cypher" => Some(Self::Cypher),
            "parquet" => Some(Self::Parquet),
            "graphml" => Some(Self::Graphml),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            Self::Csv => "csv",
            Self::Cypher => "cypher",
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
        Value::Date(_)
        | Value::LocalDateTime(_)
        | Value::DateTime { .. }
        | Value::Duration { .. }
        // `Err` is a DateTime offset outside ±18:00, which the store never
        // holds (it holds no format-V11 value); no tag is written for it.
        | Value::Binary(_) => mushroomdb::tagged_json(v)
            .ok()
            .flatten()
            .unwrap_or(serde_json::Value::Null),
        // Never stored, so an export never meets one; render it as JSON null.
        Value::Null => serde_json::Value::Null,
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
            // A property named like one of the line's own fields goes in a
            // `$props` object, so that no property is overwritten and the
            // importer can give every one back.
            let mut reserved = serde_json::Map::new();
            for (k, v) in &node.props {
                if matches!(k.as_str(), "key" | "label") || k == crate::import::JSONL_PROPS_FIELD {
                    reserved.insert(k.clone(), value_to_json(v));
                } else {
                    obj.insert(k.clone(), value_to_json(v));
                }
            }
            if !reserved.is_empty() {
                obj.insert(
                    crate::import::JSONL_PROPS_FIELD.into(),
                    serde_json::Value::Object(reserved),
                );
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

    write_rules_jsonl(rules, dest)
}

/// Write `rules.jsonl` to `dest`, one rule per line, which `mushroomdb import
/// --rules-jsonl` reads.
fn write_rules_jsonl(rules: &[RuleDef], dest: &Path) -> Result<(), crate::CliError> {
    let mut f = std::fs::File::create(dest.join("rules.jsonl"))?;
    for rule in rules {
        let line = serde_json::to_string(rule)
            .map_err(|e| crate::CliError(format!("json encode rule: {e}")))?;
        writeln!(f, "{line}")?;
    }
    Ok(())
}

// ── CSV ──────────────────────────────────────────────────────────────────

/// What a CSV property column holds. One column holds one kind; a property
/// that is more than one kind across the store's nodes, a list or a map, or a
/// string that is empty (an empty cell is an absent property), is `Json`: each
/// cell is the JSON text of the value, with the typed forms for the new types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CsvKind {
    Str,
    Int,
    Float,
    Bool,
    Date,
    LocalDateTime,
    DateTime,
    Duration,
    Binary,
    Json,
}

impl CsvKind {
    fn suffix(self) -> &'static str {
        match self {
            Self::Str => "string",
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "boolean",
            Self::Date => "date",
            Self::LocalDateTime => "localdatetime",
            Self::DateTime => "datetime",
            Self::Duration => "duration",
            Self::Binary => "binary",
            Self::Json => "json",
        }
    }

    /// The kind of one value, or `Json` when no plain column can carry it.
    fn of(v: &Value) -> Self {
        match v {
            // Zero-length text is an absent property in a CSV cell.
            Value::Str(s) if s.is_empty() => Self::Json,
            Value::Str(_) => Self::Str,
            Value::Int(_) => Self::Int,
            // A NaN or infinity has no CSV or JSON form; the export reports it.
            Value::Float(f) if f.is_finite() => Self::Float,
            Value::Bool(_) => Self::Bool,
            Value::Date(_) => Self::Date,
            Value::LocalDateTime(_) => Self::LocalDateTime,
            Value::DateTime { .. } => Self::DateTime,
            Value::Duration { .. } => Self::Duration,
            Value::Binary(_) => Self::Binary,
            Value::Float(_) | Value::List(_) | Value::Map(_) | Value::Null => Self::Json,
        }
    }
}

/// The text of a typed value's tagged form: `2024-01-15` for `{"$date": …}`.
fn tagged_text_of(v: &Value) -> String {
    match mushroomdb::tagged_json(v) {
        Ok(Some(serde_json::Value::Object(obj))) => obj
            .into_iter()
            .next()
            .and_then(|(_, text)| text.as_str().map(str::to_string))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn csv_cell(v: &Value, kind: CsvKind) -> String {
    match (kind, v) {
        (CsvKind::Str, Value::Str(s)) => s.clone(),
        (CsvKind::Int, Value::Int(i)) => i.to_string(),
        // `{:?}` is the shortest text that reads back as the same float, and
        // always has a `.` or exponent.
        (CsvKind::Float, Value::Float(f)) => format!("{f:?}"),
        (CsvKind::Bool, Value::Bool(b)) => b.to_string(),
        (CsvKind::Json, v) => serde_json::to_string(&value_to_json(v)).unwrap_or_default(),
        (_, v) => tagged_text_of(v),
    }
}

fn csv_error(e: csv::Error) -> crate::CliError {
    crate::CliError(format!("csv write error: {e}"))
}

/// Write `nodes.csv` and `relationships.csv` to `dest`, plus `rules.jsonl`
/// when there are rules.
///
/// Manual edges only: a derived edge belongs to its rule and is made again
/// when the rule is created, so importing `rules.jsonl` last restores it.
/// Edges carry no properties, so `relationships.csv` has none.
///
/// The key column is `:ID` (the key is also stored as the property `id`, as
/// `mushroomdb import` does) when every node has an `id` property equal to its
/// key, and `:KEY` (the key only) otherwise, so that an import gives back
/// exactly the properties the store had.
pub fn write_csv(
    nodes: &[NodeInfo],
    edges: &[ExportEdge],
    rules: &[RuleDef],
    dest: &Path,
) -> Result<(), crate::CliError> {
    std::fs::create_dir_all(dest)?;
    if let Some(n) = nodes.iter().find(|n| n.label.contains(';')) {
        return Err(crate::CliError(format!(
            "label `{}` of node `{}` cannot be written to CSV: the importer reads `;` as a multi-label",
            n.label, n.key
        )));
    }
    let id_is_key = !nodes.is_empty()
        && nodes
            .iter()
            .all(|n| matches!(n.props.get("id"), Some(Value::Str(s)) if *s == n.key));

    // One kind per property; a second kind turns the column into JSON.
    let mut kinds: BTreeMap<&str, CsvKind> = BTreeMap::new();
    for node in nodes {
        for (name, value) in &node.props {
            if id_is_key && name == "id" {
                continue;
            }
            if name.is_empty() {
                return Err(crate::CliError(format!(
                    "node `{}` has a property with an empty name, which a CSV header cannot carry",
                    node.key
                )));
            }
            let kind = CsvKind::of(value);
            kinds
                .entry(name)
                .and_modify(|k| {
                    if *k != kind {
                        *k = CsvKind::Json;
                    }
                })
                .or_insert(kind);
        }
    }

    // ── nodes.csv ──────────────────────────────────────────────────────────
    let mut w = csv::Writer::from_path(dest.join("nodes.csv")).map_err(csv_error)?;
    let mut header = vec![
        if id_is_key { ":ID" } else { ":KEY" }.to_string(),
        ":LABEL".to_string(),
    ];
    // Every column names its type, even a string: a property name with a colon
    // in it would otherwise be read as `name:type`.
    header.extend(kinds.iter().map(|(n, k)| format!("{n}:{}", k.suffix())));
    w.write_record(&header).map_err(csv_error)?;
    for node in nodes {
        let mut row = vec![node.key.clone(), node.label.clone()];
        for (name, kind) in &kinds {
            row.push(
                node.props
                    .get(*name)
                    .map(|v| csv_cell(v, *kind))
                    .unwrap_or_default(),
            );
        }
        w.write_record(&row).map_err(csv_error)?;
    }
    w.flush()?;

    // ── relationships.csv ──────────────────────────────────────────────────
    let mut w = csv::Writer::from_path(dest.join("relationships.csv")).map_err(csv_error)?;
    w.write_record([":START_ID", ":END_ID", ":TYPE"])
        .map_err(csv_error)?;
    for edge in edges.iter().filter(|e| !e.derived) {
        w.write_record([&edge.src, &edge.dst, &edge.edge_type])
            .map_err(csv_error)?;
    }
    w.flush()?;

    if !rules.is_empty() {
        write_rules_jsonl(rules, dest)?;
    }
    Ok(())
}

// ── Cypher ───────────────────────────────────────────────────────────────

/// What a Cypher export could not write, counted; the statements it did write
/// are all valid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CypherLoss {
    /// Properties of the new types (date, date-time, duration, binary), which
    /// no Cypher statement can store yet.
    pub typed_properties: usize,
    /// Properties holding a NaN or an infinity, or the integer `-2^63`, which
    /// have no Cypher literal.
    pub unwritable_numbers: usize,
    /// Nodes whose `id` property is not their key. A Cypher `MERGE` stores the
    /// key as `id`, so a different `id` cannot be written.
    pub differing_ids: usize,
}

impl CypherLoss {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A backtick-quoted identifier.
fn cypher_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// A single-quoted string literal on one line.
fn cypher_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// A value as a Cypher literal, or why it has none.
fn cypher_literal(v: &Value) -> Result<String, CypherSkip> {
    Ok(match v {
        Value::Str(s) => cypher_string(s),
        // `-9223372036854775808` does not lex: the digits are read before the sign.
        Value::Int(i64::MIN) => return Err(CypherSkip::Number),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if !f.is_finite() => return Err(CypherSkip::Number),
        Value::Float(f) => {
            // The lexer has no exponent form, and `{}` never writes one.
            let text = format!("{f}");
            if text.contains('.') {
                text
            } else {
                format!("{text}.0")
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::List(xs) => {
            let items: Result<Vec<String>, _> = xs.iter().map(cypher_literal).collect();
            format!("[{}]", items?.join(", "))
        }
        Value::Map(m) => {
            let mut items = Vec::with_capacity(m.len());
            for (k, v) in m {
                items.push(format!("{}: {}", cypher_ident(k), cypher_literal(v)?));
            }
            format!("{{{}}}", items.join(", "))
        }
        Value::Date(_)
        | Value::LocalDateTime(_)
        | Value::DateTime { .. }
        | Value::Duration { .. }
        | Value::Binary(_) => return Err(CypherSkip::Typed),
        Value::Null => return Err(CypherSkip::Number),
    })
}

enum CypherSkip {
    Typed,
    Number,
}

/// Write `graph.cypher` to `dest`: one statement per line, no terminator, the
/// way `mushroomdb query --file` and `--stdin` take one. Nodes first, then
/// manual edges.
///
/// ```text
/// MERGE (n:`Person` {id: 'alice'}) ON CREATE SET n.age = 30, n.tags = ['a', 'b']
/// MERGE (a:`Person` {id: 'alice'})-[:`KNOWS`]->(b:`Person` {id: 'bob'})
/// ```
///
/// `MERGE` rather than `CREATE`, so that running the file twice is harmless.
/// Cypher stores the key of a node as its `id` property, so a node whose
/// properties had no `id` gains one: the file reproduces the store exactly when
/// every node's `id` equals its key (what `mushroomdb import` and `ingest`
/// produce). Rules are not written: Cypher has no statement for one.
pub fn write_cypher(
    nodes: &[NodeInfo],
    edges: &[ExportEdge],
    dest: &Path,
) -> Result<CypherLoss, crate::CliError> {
    std::fs::create_dir_all(dest)?;
    let mut out = std::io::BufWriter::new(std::fs::File::create(dest.join("graph.cypher"))?);
    let mut loss = CypherLoss::default();
    let mut label_of: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::with_capacity(nodes.len());
    for node in nodes {
        label_of.insert(&node.key, &node.label);
        let mut sets = Vec::new();
        for (name, value) in &node.props {
            if name == "id" {
                // The statement stores the key under `id` by itself.
                if !matches!(value, Value::Str(s) if *s == node.key) {
                    loss.differing_ids += 1;
                }
                continue;
            }
            match cypher_literal(value) {
                Ok(lit) => sets.push(format!("n.{} = {lit}", cypher_ident(name))),
                Err(CypherSkip::Typed) => loss.typed_properties += 1,
                Err(CypherSkip::Number) => loss.unwritable_numbers += 1,
            }
        }
        write!(
            out,
            "MERGE (n:{} {{id: {}}})",
            cypher_ident(&node.label),
            cypher_string(&node.key)
        )?;
        if !sets.is_empty() {
            write!(out, " ON CREATE SET {}", sets.join(", "))?;
        }
        writeln!(out)?;
    }
    for edge in edges.iter().filter(|e| !e.derived) {
        let (Some(src_label), Some(dst_label)) = (
            label_of.get(edge.src.as_str()),
            label_of.get(edge.dst.as_str()),
        ) else {
            continue;
        };
        writeln!(
            out,
            "MERGE (a:{} {{id: {}}})-[:{}]->(b:{} {{id: {}}})",
            cypher_ident(src_label),
            cypher_string(&edge.src),
            cypher_ident(&edge.edge_type),
            cypher_ident(dst_label),
            cypher_string(&edge.dst)
        )?;
    }
    out.flush()?;
    Ok(loss)
}

/// How many float values in `nodes` are NaN or infinite, which neither JSON nor
/// CSV can write. They are left out (JSON writes `null`); the export says how
/// many.
pub fn count_non_finite(nodes: &[NodeInfo]) -> usize {
    fn count(v: &Value) -> usize {
        match v {
            Value::Float(f) => usize::from(!f.is_finite()),
            Value::List(xs) => xs.iter().map(count).sum(),
            Value::Map(m) => m.values().map(count).sum(),
            _ => 0,
        }
    }
    nodes.iter().flat_map(|n| n.props.values()).map(count).sum()
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
    /// `Value::Int` is a 64-bit `i64` (`core_storage::Value::Int`). GraphML's
    /// informal convention treats `attr.type="int"` as 32-bit, which cannot
    /// represent the full range; `"long"` is the interoperable choice for a
    /// 64-bit integer and is what this writer declares.
    Long,
    Double,
    String,
}

impl GmlType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Long => "long",
            Self::Double => "double",
            Self::String => "string",
        }
    }
}

/// The GraphML attribute type for a scalar `Value`. Lists and maps are
/// JSON-encoded text, so they declare `string`.
fn value_gml_type(v: &Value) -> GmlType {
    match v {
        Value::Int(_) => GmlType::Long,
        Value::Float(_) => GmlType::Double,
        Value::Bool(_) => GmlType::Boolean,
        Value::Str(_) | Value::List(_) | Value::Map(_) => GmlType::String,
        // Tagged JSON text until GraphML gets typed temporal attributes.
        Value::Date(_)
        | Value::LocalDateTime(_)
        | Value::DateTime { .. }
        | Value::Duration { .. }
        | Value::Binary(_) => GmlType::String,
        Value::Null => GmlType::String,
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
/// original name is preserved in `attr.name`. A node property literally
/// named `label` shares the `n_label` key (and `<data>` slot) with the
/// built-in label field — an accepted collision, since mushroomdb's schema
/// reserves `label` for the node's type name.
///
/// **Type declaration:** a node property's `attr.type` is [`value_gml_type`]
/// of its value where every node reporting that property name agrees on the
/// `Value` variant; if two nodes disagree (e.g. one has `Value::Int` under
/// `score`, another `Value::Str`), the key declares `attr.type="string"` for
/// every node — a safe fallback, since `string` can hold any value's text
/// form — rather than picking one node's type and risking a value that
/// doesn't fit it. `Value::Int` (a 64-bit `i64`) declares `attr.type="long"`,
/// not `"int"`, since GraphML's informal convention treats `"int"` as
/// 32-bit and `"long"` is the interoperable choice for the full range.
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
    // `Some(t)` while every node reporting this name agrees on type `t`;
    // `None` once two nodes disagree — resolved to `string` below.
    let mut node_key_types: BTreeMap<String, Option<GmlType>> = BTreeMap::new();
    node_key_types.insert("label".to_string(), Some(GmlType::String));
    for node in nodes {
        for (k, v) in &node.props {
            let t = value_gml_type(v);
            node_key_types
                .entry(k.clone())
                .and_modify(|existing| {
                    if *existing != Some(t) {
                        *existing = None;
                    }
                })
                .or_insert(Some(t));
        }
    }
    let node_key_types: BTreeMap<String, GmlType> = node_key_types
        .into_iter()
        .map(|(k, t)| (k, t.unwrap_or(GmlType::String)))
        .collect();

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

#[cfg(test)]
mod v11_value_tests {
    use super::{value_gml_text, value_gml_type, value_to_json, GmlType};
    use mushroomdb::{json_to_value, Value};
    use serde_json::json;

    /// Export writes the tagged wire form, so an exported store re-imports
    /// with its types intact.
    #[test]
    fn export_writes_the_tagged_form_that_reimports() {
        let date = Value::Date(19_737);
        assert_eq!(value_to_json(&date), json!({"$date": "2024-01-15"}));
        assert_eq!(json_to_value(value_to_json(&date)), Ok(Some(date.clone())));
        let blob = Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(json_to_value(value_to_json(&blob)), Ok(Some(blob.clone())));
        // GraphML carries the tagged JSON text in a string attribute.
        assert!(matches!(value_gml_type(&date), GmlType::String));
        assert_eq!(value_gml_text(&date), r#"{"$date":"2024-01-15"}"#);
    }
}

#[cfg(test)]
mod round_trip_unit_tests {
    use super::{cypher_literal, cypher_string, write_csv, CypherSkip};
    use crate::import::{parse_node_header, parse_node_row};
    use mushroomdb::{NodeInfo, Value};
    use std::collections::BTreeMap;

    fn typed_values() -> Vec<(&'static str, Value)> {
        vec![
            ("date", Value::Date(19_737)),
            ("ldt", Value::LocalDateTime(1_705_322_096_789_012)),
            (
                "dt",
                Value::DateTime {
                    micros_utc: 1_705_302_296_789_012,
                    offset_seconds: 19_800,
                },
            ),
            (
                "dur",
                Value::Duration {
                    months: 14,
                    days: 3,
                    nanos: 1,
                },
            ),
            ("bin", Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF])),
            // A typed value inside a list or a map goes through a json column.
            (
                "nested",
                Value::List(vec![
                    Value::Date(0),
                    Value::Map(BTreeMap::from([("b".to_string(), Value::Binary(vec![1]))])),
                ]),
            ),
            ("whole", Value::Float(3.0)),
            ("empty", Value::Str(String::new())),
        ]
    }

    /// The new value types come back from a CSV export with their types, through
    /// the importer's own header and row readers (a store cannot hold one until
    /// V11 lands, so this is the file-level proof).
    #[test]
    fn csv_export_of_the_new_types_reads_back_typed() {
        let dir = std::env::temp_dir().join(format!("mushroomdb-csv-typed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let node = NodeInfo {
            key: "n1".into(),
            label: "T".into(),
            props: typed_values()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        };
        write_csv(std::slice::from_ref(&node), &[], &[], &dir).unwrap();

        let mut reader = csv::Reader::from_path(dir.join("nodes.csv")).unwrap();
        let header = reader.headers().unwrap().clone();
        assert_eq!(&header[0], ":KEY");
        let schema = parse_node_header(&header).unwrap();
        assert!(schema.warnings.is_empty(), "{:?}", schema.warnings);
        let record = reader.records().next().unwrap().unwrap();
        let (label, props) = parse_node_row(&schema, &record, ';').unwrap();
        assert_eq!(label, "T");
        assert_eq!(props, node.props);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cypher_literals_have_no_exponent_and_refuse_what_has_no_literal() {
        let lit = |v: Value| cypher_literal(&v).ok();
        assert_eq!(lit(Value::Float(1.0)).as_deref(), Some("1.0"));
        assert_eq!(lit(Value::Float(-0.5)).as_deref(), Some("-0.5"));
        let huge = lit(Value::Float(1e300)).unwrap();
        assert!(!huge.contains('e') && huge.ends_with(".0"), "{huge}");
        assert_eq!(lit(Value::Int(-5)).as_deref(), Some("-5"));
        assert!(matches!(
            cypher_literal(&Value::Int(i64::MIN)),
            Err(CypherSkip::Number)
        ));
        assert!(matches!(
            cypher_literal(&Value::Float(f64::NAN)),
            Err(CypherSkip::Number)
        ));
        assert!(matches!(
            cypher_literal(&Value::List(vec![Value::Date(1)])),
            Err(CypherSkip::Typed)
        ));
        assert_eq!(
            lit(Value::List(vec![Value::Int(1), Value::Str("a".into())])).as_deref(),
            Some("[1, 'a']")
        );
    }

    #[test]
    fn cypher_strings_stay_on_one_line() {
        assert_eq!(
            cypher_string("it's \\ \n\r\t\u{1}"),
            r"'it\'s \\ \n\r\t\u0001'"
        );
        assert!(!cypher_string("a\nb").contains('\n'));
    }
}
