//! `mushroomdb import`: bulk load of a nodes CSV and a relationships CSV.
//!
//! The header conventions are the widely used bulk-import ones:
//!
//! * nodes: `:ID` (the node key), `:LABEL`, then property columns;
//! * relationships: `:START_ID`, `:END_ID`, `:TYPE`, then property columns;
//! * a property column may carry a type suffix: `age:int`, `weight:float`,
//!   `active:boolean`, `tags:string[]`. No suffix means a string.
//!
//! The data flows through the engine's ordinary batch path
//! ([`GraphDb::ingest`] for nodes, [`GraphDb::write_batch`] for edges), one
//! WAL frame and one fsync per batch, so everything the engine guarantees about
//! a committed batch holds here. What the import adds is what it leaves out:
//! no auto-declared foreign-key rules ([`AutoFk::Off`]), no per-row dedupe round
//! trips beyond one key lookup, and a single snapshot at the end.
//!
//! # Rules
//!
//! The import never declares a rule. Rules the store already has fire on each
//! batch exactly as they do for any other write, so the final store is the one
//! incremental ingest of the same rows produces. The fast route is to import
//! into a store with no rules and declare them afterwards: `create_rule`
//! backfills in one pass, and the differential test in
//! `crates/cli/tests/csv_import.rs` pins that the two routes derive the same
//! edges.
//!
//! # Crash safety
//!
//! Every batch is one atomic WAL frame, so an import killed part-way leaves a
//! store that opens, holding every batch that committed and none of the one in
//! flight. Derived (rule-owned) edges are not in the WAL; they are rebuilt on
//! open from the base data, so an interrupted import loses no base data and
//! the derived edges are whatever the committed base data derives. The
//! snapshot that ends a successful import is written last; an import that dies
//! before it simply has a longer WAL to replay. The import is not
//! all-or-nothing: batches committed before an error or a crash stay.

use crate::CliError;
use mushroomdb::{AutoFk, GraphDb, IngestOptions, RealFs, Value};

type Db = GraphDb<RealFs>;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Default rows per committed batch.
pub const DEFAULT_BATCH_SIZE: usize = 50_000;

/// How many skipped-row messages a report keeps; the count is always exact.
const MAX_REPORTED_ERRORS: usize = 100;

/// Options for [`run_import`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOptions {
    /// Field delimiter of both files.
    pub delimiter: u8,
    /// Separator between the elements of an array cell (`tags:string[]`), and
    /// the character that marks a multi-label `:LABEL` cell, which is refused.
    pub array_delimiter: char,
    /// Skip a bad row (counted and reported) instead of stopping at it.
    pub ignore_errors: bool,
    /// Rows per committed batch.
    pub batch_size: usize,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            array_delimiter: ';',
            ignore_errors: false,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

/// What an import did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub nodes_inserted: usize,
    pub edges_inserted: usize,
    /// Rows skipped under `--ignore-errors` (exact).
    pub rows_skipped: usize,
    /// The first skipped rows, each `file:line: reason`.
    pub errors: Vec<String>,
    /// Header warnings, each `file:line: reason`.
    pub warnings: Vec<String>,
    /// WAL batches committed.
    pub batches: usize,
    /// Rules created from a rules file.
    pub rules_created: usize,
    /// `edges.jsonl` lines marked `"derived": true`. A derived edge belongs to
    /// the rule that makes it and is made again when the rule is created, so it
    /// is not inserted.
    pub derived_edges_skipped: usize,
}

/// One committed batch, reported to the callback of [`run_import_with_progress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchProgress {
    /// Batches committed so far, this one included.
    pub batches_committed: usize,
    pub nodes_inserted: usize,
    pub edges_inserted: usize,
}

/// The scalar type of a property column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarType {
    Str,
    Int,
    Float,
    Bool,
    /// A JSON document in the cell: a list, a map, or a value of any type, in
    /// the wire forms of `docs/site/http.md` (a date is `{"$date": "…"}`).
    Json,
    /// The four temporal types and binary, in the text their tagged form
    /// carries: `2024-01-15`, `2024-01-15T10:30:00`, `2024-01-15T10:30:00+05:30`,
    /// `P1DT2H`, and base64.
    Date,
    LocalDateTime,
    DateTime,
    Duration,
    Binary,
}

impl ScalarType {
    /// The tag of the wire form this type's cell text is the payload of.
    fn tag(self) -> Option<&'static str> {
        match self {
            Self::Date => Some("$date"),
            Self::LocalDateTime => Some("$ldt"),
            Self::DateTime => Some("$dt"),
            Self::Duration => Some("$dur"),
            Self::Binary => Some("$binary"),
            _ => None,
        }
    }
}

/// A property column's declared type: a scalar, or an array of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnType {
    pub scalar: ScalarType,
    pub array: bool,
}

/// One property column of a header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropColumn {
    pub name: String,
    pub index: usize,
    pub ty: ColumnType,
}

/// A parsed nodes-file header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSchema {
    /// The property the node key is also stored under: the `:ID` column's name
    /// (`personId:ID`), or `id` when the column has none.
    pub key_field: String,
    /// False for a `:KEY` column: the cell is the node key and is not also
    /// stored as a property (an exported store whose nodes do not all carry
    /// their key as `id` is written this way so that it re-imports exactly).
    pub key_stored: bool,
    pub id_index: usize,
    pub label_index: usize,
    pub props: Vec<PropColumn>,
    pub width: usize,
    /// Non-fatal header findings (an unknown type suffix), without file prefix.
    pub warnings: Vec<String>,
}

/// A parsed relationships-file header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelSchema {
    pub start_index: usize,
    pub end_index: usize,
    pub type_index: usize,
    pub props: Vec<PropColumn>,
    pub width: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Column {
    Id(String),
    Key,
    Label,
    StartId,
    EndId,
    RelType,
    Ignore,
    Prop(PropColumn),
}

/// Split one header cell into its column meaning.
fn parse_column(cell: &str, index: usize, warnings: &mut Vec<String>) -> Result<Column, String> {
    let cell = if index == 0 {
        cell.trim_start_matches('\u{feff}')
    } else {
        cell
    };
    let Some(colon) = cell.rfind(':') else {
        if cell.is_empty() {
            return Err(format!("column {} has an empty name", index + 1));
        }
        return Ok(Column::Prop(PropColumn {
            name: cell.to_string(),
            index,
            ty: ColumnType {
                scalar: ScalarType::Str,
                array: false,
            },
        }));
    };
    let name = &cell[..colon];
    let suffix = &cell[colon + 1..];
    let upper = suffix.to_ascii_uppercase();
    let special = upper.split('(').next().unwrap_or("");
    // `:ID(Space)` names an id space, so the same id can be a different node
    // in each. Every id here lives in one key space; reading the column as a
    // plain `:ID` would refuse the repeats as duplicates, with the wrong reason.
    if matches!(special, "ID" | "START_ID" | "END_ID") && upper.contains('(') {
        return Err(format!(
            "column {} `{cell}`: id spaces are not supported in mushroomdb 0.10; \
             use `{}` and keep ids unique across the whole file",
            index + 1,
            &cell[..colon + 1 + special.len()]
        ));
    }
    match special {
        "ID" => return Ok(Column::Id(name.to_string())),
        "KEY" if name.is_empty() => return Ok(Column::Key),
        "LABEL" if name.is_empty() => return Ok(Column::Label),
        "START_ID" => return Ok(Column::StartId),
        "END_ID" => return Ok(Column::EndId),
        "TYPE" if name.is_empty() => return Ok(Column::RelType),
        "IGNORE" => return Ok(Column::Ignore),
        _ => {}
    }
    if name.is_empty() {
        return Err(format!(
            "column {} `{cell}` has no name before its type",
            index + 1
        ));
    }
    let (base, array) = match upper.strip_suffix("[]") {
        Some(b) => (b.to_string(), true),
        None => (upper.clone(), false),
    };
    let scalar = match base.as_str() {
        "STRING" | "CHAR" | "" => ScalarType::Str,
        "INT" | "INTEGER" | "LONG" | "SHORT" | "BYTE" => ScalarType::Int,
        "FLOAT" | "DOUBLE" => ScalarType::Float,
        "BOOLEAN" | "BOOL" => ScalarType::Bool,
        "JSON" => ScalarType::Json,
        "DATE" => ScalarType::Date,
        "LOCALDATETIME" => ScalarType::LocalDateTime,
        "DATETIME" => ScalarType::DateTime,
        "DURATION" => ScalarType::Duration,
        "BINARY" => ScalarType::Binary,
        _ => {
            warnings.push(format!(
                "column {} `{cell}`: unknown type `{suffix}`, stored as a string",
                index + 1
            ));
            ScalarType::Str
        }
    };
    Ok(Column::Prop(PropColumn {
        name: name.to_string(),
        index,
        ty: ColumnType { scalar, array },
    }))
}

struct Columns {
    id: Vec<(usize, String)>,
    key: Vec<usize>,
    label: Vec<usize>,
    start: Vec<usize>,
    end: Vec<usize>,
    rel_type: Vec<usize>,
    props: Vec<PropColumn>,
    warnings: Vec<String>,
}

fn classify_header(record: &csv::StringRecord) -> Result<Columns, String> {
    let mut cols = Columns {
        id: Vec::new(),
        key: Vec::new(),
        label: Vec::new(),
        start: Vec::new(),
        end: Vec::new(),
        rel_type: Vec::new(),
        props: Vec::new(),
        warnings: Vec::new(),
    };
    for (index, cell) in record.iter().enumerate() {
        match parse_column(cell, index, &mut cols.warnings)? {
            Column::Id(name) => cols.id.push((index, name)),
            Column::Key => cols.key.push(index),
            Column::Label => cols.label.push(index),
            Column::StartId => cols.start.push(index),
            Column::EndId => cols.end.push(index),
            Column::RelType => cols.rel_type.push(index),
            Column::Ignore => {}
            Column::Prop(p) => cols.props.push(p),
        }
    }
    for p in &cols.props {
        if p.ty.array && p.ty.scalar == ScalarType::Json {
            return Err(format!(
                "property column `{}`: json cannot be an array column; a json cell holds the whole list",
                p.name
            ));
        }
    }
    let mut seen = HashSet::new();
    for p in &cols.props {
        if !seen.insert(p.name.as_str()) {
            return Err(format!("property column `{}` appears twice", p.name));
        }
    }
    Ok(cols)
}

fn exactly_one(found: &[usize], what: &str) -> Result<usize, String> {
    match found {
        [one] => Ok(*one),
        [] => Err(format!("the header has no {what} column")),
        _ => Err(format!("the header has more than one {what} column")),
    }
}

/// Parse a nodes-file header. Needs exactly one `:ID` and one `:LABEL`.
pub fn parse_node_header(record: &csv::StringRecord) -> Result<NodeSchema, String> {
    let cols = classify_header(record)?;
    if !cols.start.is_empty() || !cols.end.is_empty() || !cols.rel_type.is_empty() {
        return Err(
            "a nodes file has a relationship column (:START_ID, :END_ID or :TYPE)".to_string(),
        );
    }
    let id_indexes: Vec<usize> = cols
        .id
        .iter()
        .map(|(i, _)| *i)
        .chain(cols.key.iter().copied())
        .collect();
    let id_index = exactly_one(&id_indexes, ":ID")?;
    let label_index = exactly_one(&cols.label, ":LABEL")?;
    let key_stored = cols.key.is_empty();
    let key_field = match cols.id.first().map(|(_, n)| n.as_str()) {
        Some("") | None => "id".to_string(),
        Some(name) => name.to_string(),
    };
    if key_stored && cols.props.iter().any(|p| p.name == key_field) {
        return Err(format!(
            "property column `{key_field}` has the same name as the :ID column"
        ));
    }
    Ok(NodeSchema {
        key_field,
        key_stored,
        id_index,
        label_index,
        props: cols.props,
        width: record.len(),
        warnings: cols.warnings,
    })
}

/// Parse a relationships-file header. Needs exactly one each of `:START_ID`,
/// `:END_ID` and `:TYPE`.
pub fn parse_rel_header(record: &csv::StringRecord) -> Result<RelSchema, String> {
    let cols = classify_header(record)?;
    if !cols.id.is_empty() || !cols.key.is_empty() || !cols.label.is_empty() {
        return Err("a relationships file has a node column (:ID or :LABEL)".to_string());
    }
    Ok(RelSchema {
        start_index: exactly_one(&cols.start, ":START_ID")?,
        end_index: exactly_one(&cols.end, ":END_ID")?,
        type_index: exactly_one(&cols.rel_type, ":TYPE")?,
        props: cols.props,
        width: record.len(),
        warnings: cols.warnings,
    })
}

fn parse_scalar(scalar: ScalarType, text: &str) -> Result<Value, String> {
    match scalar {
        ScalarType::Str => Ok(Value::Str(text.to_string())),
        ScalarType::Int => text
            .trim()
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| format!("`{text}` is not an integer")),
        ScalarType::Float => match text.trim().parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Value::Float(f)),
            _ => Err(format!("`{text}` is not a finite float")),
        },
        ScalarType::Bool => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(format!("`{text}` is not a boolean (true or false)")),
        },
        ScalarType::Json => {
            let json: serde_json::Value =
                serde_json::from_str(text).map_err(|e| format!("`{text}` is not JSON: {e}"))?;
            json_to_exact_value(json)
                .and_then(|v| v.ok_or_else(|| "a JSON null is not a value".to_string()))
        }
        typed => {
            let tag = typed.tag().unwrap_or("$date");
            let mut tagged = serde_json::Map::with_capacity(1);
            tagged.insert(tag.to_string(), serde_json::Value::String(text.to_string()));
            mushroomdb::json_to_value(serde_json::Value::Object(tagged))?
                .ok_or_else(|| format!("`{text}` is not a value"))
        }
    }
}

fn parse_cell(col: &PropColumn, text: &str, array_delimiter: char) -> Result<Value, String> {
    let located = |e: String| format!("column `{}`: {e}", col.name);
    if !col.ty.array {
        return parse_scalar(col.ty.scalar, text).map_err(located);
    }
    let mut items = Vec::new();
    for part in text.split(array_delimiter) {
        items.push(parse_scalar(col.ty.scalar, part).map_err(located)?);
    }
    Ok(Value::List(items))
}

/// Parse one node row into its label and property map, key included under
/// `schema.key_field`. An empty property cell is an absent property.
pub fn parse_node_row(
    schema: &NodeSchema,
    record: &csv::StringRecord,
    array_delimiter: char,
) -> Result<(String, BTreeMap<String, Value>), String> {
    if record.len() != schema.width {
        return Err(format!(
            "expected {} fields, found {}",
            schema.width,
            record.len()
        ));
    }
    let id = &record[schema.id_index];
    if id.is_empty() {
        return Err("empty :ID".to_string());
    }
    // An empty `:LABEL` cell is a node with no label (QL-NM), which is how
    // `export` writes one.
    let label = &record[schema.label_index];
    if label.contains(array_delimiter) || label.contains(';') {
        return Err(format!(
            "`{label}` has more than one label; mushroomdb stores one label per node"
        ));
    }
    let mut row = BTreeMap::new();
    if schema.key_stored {
        row.insert(schema.key_field.clone(), Value::Str(id.to_string()));
    }
    for col in &schema.props {
        let text = &record[col.index];
        if text.is_empty() {
            continue;
        }
        row.insert(col.name.clone(), parse_cell(col, text, array_delimiter)?);
    }
    Ok((label.to_string(), row))
}

/// Parse one relationship row into `(type, start, end)`.
pub fn parse_rel_row(
    schema: &RelSchema,
    record: &csv::StringRecord,
) -> Result<(String, String, String), String> {
    if record.len() != schema.width {
        return Err(format!(
            "expected {} fields, found {}",
            schema.width,
            record.len()
        ));
    }
    let start = &record[schema.start_index];
    let end = &record[schema.end_index];
    let edge_type = &record[schema.type_index];
    if start.is_empty() {
        return Err("empty :START_ID".to_string());
    }
    if end.is_empty() {
        return Err("empty :END_ID".to_string());
    }
    if edge_type.is_empty() {
        return Err("empty :TYPE".to_string());
    }
    Ok((edge_type.to_string(), start.to_string(), end.to_string()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    FieldStart,
    Unquoted,
    Quoted,
    Closed,
}

/// Check that every quote in `input` follows RFC 4180: a quoted field opens at
/// the start of a field, `""` is a literal quote inside it, and the closing
/// quote is followed by a delimiter or a line end. Returns the message, with its
/// line, for the first violation.
///
/// The `csv` parser is deliberately lenient: an unterminated quote swallows the
/// rest of the file into one field and a stray quote is literal text. Either
/// would load wrong data without a word, and neither can be skipped row by row
/// because the row boundaries are unknowable, so a file is checked whole before
/// anything is written and a violation is fatal, `--ignore-errors` or not.
pub fn check_quoting<R: std::io::Read>(mut input: R, delimiter: u8) -> Result<(), String> {
    let mut state = Quote::FieldStart;
    let mut line = 1u64;
    let mut opened_at = 1u64;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = input.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            state = match (state, b) {
                (Quote::FieldStart, b'"') => {
                    opened_at = line;
                    Quote::Quoted
                }
                (Quote::Unquoted, b'"') => {
                    return Err(format!("{line}: a quote inside an unquoted field"));
                }
                (Quote::Quoted, b'"') => Quote::Closed,
                (Quote::Closed, b'"') => Quote::Quoted,
                (Quote::Closed, b'\r') => Quote::Closed,
                (Quote::Closed, b'\n') => {
                    line += 1;
                    Quote::FieldStart
                }
                (Quote::Closed, d) if d == delimiter => Quote::FieldStart,
                (Quote::Closed, _) => {
                    return Err(format!(
                        "{line}: text after the closing quote of a quoted field"
                    ));
                }
                (Quote::Quoted, b'\n') => {
                    line += 1;
                    Quote::Quoted
                }
                (Quote::Quoted, _) => Quote::Quoted,
                (_, b'\n') => {
                    line += 1;
                    Quote::FieldStart
                }
                (_, d) if d == delimiter => Quote::FieldStart,
                (_, _) => Quote::Unquoted,
            };
        }
    }
    if state == Quote::Quoted {
        return Err(format!(
            "{opened_at}: a quoted field opened here is never closed"
        ));
    }
    Ok(())
}

fn check_file_quoting(path: &Path, opts: &ImportOptions) -> Result<(), CliError> {
    let file =
        std::fs::File::open(path).map_err(|e| CliError(format!("{}: {e}", path.display())))?;
    check_quoting(file, opts.delimiter).map_err(|e| {
        CliError(format!(
            "{}:{e}; malformed quoting is fatal because row boundaries cannot be trusted, nothing was imported",
            path.display()
        ))
    })
}

/// Shared state of one import run.
struct Run<'a> {
    opts: &'a ImportOptions,
    report: ImportReport,
    on_batch: &'a mut dyn FnMut(&BatchProgress),
}

impl Run<'_> {
    fn committed(&mut self) {
        self.report.batches += 1;
        let progress = BatchProgress {
            batches_committed: self.report.batches,
            nodes_inserted: self.report.nodes_inserted,
            edges_inserted: self.report.edges_inserted,
        };
        (self.on_batch)(&progress);
    }

    /// A bad row: skip it under `--ignore-errors`, otherwise stop with it.
    fn bad_row(&mut self, file: &str, line: u64, reason: &str) -> Result<(), CliError> {
        let message = format!("{file}:{line}: {reason}");
        if !self.opts.ignore_errors {
            return Err(CliError(format!(
                "{message}\nstopped: {} nodes and {} edges in {} committed batches remain in the store \
                 (an import is not all-or-nothing); rerun with --ignore-errors to skip bad rows",
                self.report.nodes_inserted, self.report.edges_inserted, self.report.batches
            )));
        }
        self.report.rows_skipped += 1;
        if self.report.errors.len() < MAX_REPORTED_ERRORS {
            self.report.errors.push(message);
        }
        Ok(())
    }
}

fn reader_for(path: &Path, opts: &ImportOptions) -> Result<csv::Reader<std::fs::File>, CliError> {
    csv::ReaderBuilder::new()
        .delimiter(opts.delimiter)
        .has_headers(true)
        .flexible(true)
        .from_path(path)
        .map_err(|e| CliError(format!("{}: {e}", path.display())))
}

/// The line a csv error happened on, when it knows.
fn error_line(e: &csv::Error) -> u64 {
    e.position().map_or(0, csv::Position::line)
}

fn read_header(
    reader: &mut csv::Reader<std::fs::File>,
    file: &str,
) -> Result<csv::StringRecord, CliError> {
    match reader.headers() {
        Ok(h) if h.is_empty() || (h.len() == 1 && h[0].is_empty()) => {
            Err(CliError(format!("{file}:1: the file has no header row")))
        }
        Ok(h) => Ok(h.clone()),
        Err(e) => Err(CliError(format!("{file}:{}: {e}", error_line(&e).max(1)))),
    }
}

fn import_nodes(db: &mut Db, path: &Path, run: &mut Run<'_>) -> Result<(), CliError> {
    let file = path.display().to_string();
    let mut reader = reader_for(path, run.opts)?;
    let header = read_header(&mut reader, &file)?;
    let schema = parse_node_header(&header).map_err(|e| CliError(format!("{file}:1: {e}")))?;
    for w in &schema.warnings {
        run.report.warnings.push(format!("{file}:1: {w}"));
    }
    let ingest_opts = IngestOptions {
        key_field: schema.key_field.clone(),
        // Foreign-key rules are inferred from the rows; a bulk load declares
        // nothing the file did not ask for.
        auto_fk: AutoFk::Off,
    };

    // TODO(W2 unique constraints): the batch is where a declared unique
    // constraint on (label, property) would be checked, before `ingest`.
    let mut batch: HashMap<String, Vec<BTreeMap<String, Value>>> = HashMap::new();
    let mut records: Vec<NodeRecord> = Vec::new();
    let mut batch_keys: HashSet<String> = HashSet::new();
    let mut buffered = 0usize;
    let mut record = csv::StringRecord::new();
    loop {
        match reader.read_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) if matches!(e.kind(), csv::ErrorKind::Io(_)) => {
                return Err(CliError(format!("{file}: {e}")));
            }
            Err(e) => {
                run.bad_row(&file, error_line(&e), &e.to_string())?;
                continue;
            }
        }
        let line = record.position().map_or(0, csv::Position::line);
        let (label, row) = match parse_node_row(&schema, &record, run.opts.array_delimiter) {
            Ok(parsed) => parsed,
            Err(reason) => {
                run.bad_row(&file, line, &reason)?;
                continue;
            }
        };
        let key = record[schema.id_index].to_string();
        if db.has_node(&key) || !batch_keys.insert(key.clone()) {
            let reason = format!("duplicate :ID `{key}`");
            run.bad_row(&file, line, &reason)?;
            continue;
        }
        if schema.key_stored {
            batch.entry(label).or_default().push(row);
        } else {
            records.push(NodeRecord {
                label,
                key,
                props: row.into_iter().collect(),
                line,
            });
        }
        buffered += 1;
        if buffered >= run.opts.batch_size {
            flush_nodes(db, &mut batch, &ingest_opts, run)?;
            flush_node_records(db, &mut records, &file, run)?;
            batch_keys.clear();
            buffered = 0;
        }
    }
    flush_nodes(db, &mut batch, &ingest_opts, run)?;
    flush_node_records(db, &mut records, &file, run)?;
    Ok(())
}

/// One node to insert exactly as given: its key is not also a property.
#[derive(Clone)]
struct NodeRecord {
    label: String,
    key: String,
    props: Vec<(String, Value)>,
    /// Line in the file, for messages.
    line: u64,
}

fn commit_node_records(db: &mut Db, records: Vec<NodeRecord>) -> mushroomdb::Result<usize> {
    let (inserted, _) = db.write_batch(|b| {
        for r in records {
            b.insert_node(&r.label, &r.key, r.props);
        }
    })?;
    Ok(inserted)
}

/// Commit `records` as one WAL batch through the engine's ordinary batch path,
/// the one the CSV edges use. A rejected batch is handled like a rejected edge
/// batch: strict mode stops with the lines named and nothing of the batch
/// written; `--ignore-errors` retries the rows one by one and skips the ones
/// that fail alone.
fn flush_node_records(
    db: &mut Db,
    records: &mut Vec<NodeRecord>,
    file: &str,
    run: &mut Run<'_>,
) -> Result<(), CliError> {
    if records.is_empty() {
        return Ok(());
    }
    let batch = std::mem::take(records);
    let first = batch.first().map_or(0, |r| r.line);
    let last = batch.last().map_or(0, |r| r.line);
    let count = batch.len();
    // The rows are only needed again if the batch is rejected and the rows are
    // to be tried one by one.
    let retry: Option<Vec<NodeRecord>> = run.opts.ignore_errors.then(|| batch.clone());
    match commit_node_records(db, batch) {
        Ok(inserted) => {
            run.report.nodes_inserted += inserted;
            run.committed();
        }
        Err(batch_err) => {
            let Some(rows) = retry else {
                run.bad_row(
                    file,
                    first,
                    &format!(
                        "the batch of {count} nodes (lines {first}-{last}) was rejected: {batch_err}"
                    ),
                )?;
                return Ok(());
            };
            let mut any = false;
            for one in rows {
                let line = one.line;
                match commit_node_records(db, vec![one]) {
                    Ok(inserted) => {
                        run.report.nodes_inserted += inserted;
                        any = true;
                    }
                    Err(e) => run.bad_row(file, line, &e.to_string())?,
                }
            }
            if any {
                run.committed();
            }
        }
    }
    Ok(())
}

fn flush_nodes(
    db: &mut Db,
    batch: &mut HashMap<String, Vec<BTreeMap<String, Value>>>,
    ingest_opts: &IngestOptions,
    run: &mut Run<'_>,
) -> Result<(), CliError> {
    let mut labels: Vec<String> = batch.keys().cloned().collect();
    labels.sort();
    for label in labels {
        let rows = batch.remove(&label).unwrap_or_default();
        if rows.is_empty() {
            continue;
        }
        let report = db.ingest(&label, rows, ingest_opts)?;
        run.report.nodes_inserted += report.inserted;
        run.committed();
    }
    Ok(())
}

fn import_rels(db: &mut Db, path: &Path, run: &mut Run<'_>) -> Result<(), CliError> {
    let file = path.display().to_string();
    let mut reader = reader_for(path, run.opts)?;
    let header = read_header(&mut reader, &file)?;
    let schema = parse_rel_header(&header).map_err(|e| CliError(format!("{file}:1: {e}")))?;
    if let Some(p) = schema.props.first() {
        return Err(CliError(format!(
            "{file}:1: relationship property column `{}` is not supported in mushroomdb 0.10: \
             edges carry no properties; remove the column",
            p.name
        )));
    }
    for w in &schema.warnings {
        run.report.warnings.push(format!("{file}:1: {w}"));
    }

    let mut batch: Vec<(String, String, String, u64)> = Vec::new();
    let mut record = csv::StringRecord::new();
    loop {
        match reader.read_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) if matches!(e.kind(), csv::ErrorKind::Io(_)) => {
                return Err(CliError(format!("{file}: {e}")));
            }
            Err(e) => {
                run.bad_row(&file, error_line(&e), &e.to_string())?;
                continue;
            }
        }
        let line = record.position().map_or(0, csv::Position::line);
        let (edge_type, start, end) = match parse_rel_row(&schema, &record) {
            Ok(parsed) => parsed,
            Err(reason) => {
                run.bad_row(&file, line, &reason)?;
                continue;
            }
        };
        let missing = [(":START_ID", &start), (":END_ID", &end)]
            .into_iter()
            .find(|(_, id)| !db.has_node(id));
        if let Some((column, id)) = missing {
            let reason = format!("{column} `{id}` is not a node in the nodes file or the store");
            run.bad_row(&file, line, &reason)?;
            continue;
        }
        batch.push((edge_type, start, end, line));
        if batch.len() >= run.opts.batch_size {
            flush_edges(db, &mut batch, &file, run)?;
        }
    }
    flush_edges(db, &mut batch, &file, run)?;
    Ok(())
}

fn commit_edges(db: &mut Db, edges: &[(String, String, String, u64)]) -> mushroomdb::Result<usize> {
    let (_, inserted) = db.write_batch(|b| {
        for (edge_type, start, end, _) in edges {
            b.insert_edge(edge_type, start, end);
        }
    })?;
    Ok(inserted)
}

fn flush_edges(
    db: &mut Db,
    batch: &mut Vec<(String, String, String, u64)>,
    file: &str,
    run: &mut Run<'_>,
) -> Result<(), CliError> {
    if batch.is_empty() {
        return Ok(());
    }
    let edges = std::mem::take(batch);
    match commit_edges(db, &edges) {
        Ok(inserted) => {
            run.report.edges_inserted += inserted;
            run.committed();
        }
        Err(batch_err) => {
            let first = edges.first().map_or(0, |e| e.3);
            let last = edges.last().map_or(0, |e| e.3);
            if !run.opts.ignore_errors {
                // The engine rejected the batch whole and nothing of it was
                // committed; strict mode does not go looking for which row.
                run.bad_row(
                    file,
                    first,
                    &format!(
                        "the batch of {} relationships (lines {first}-{last}) was rejected: \
                         {batch_err}",
                        edges.len()
                    ),
                )?;
                return Ok(());
            }
            // Under `--ignore-errors`, find the rows that fail alone, skip them,
            // and count what the rest add as one batch.
            let mut any = false;
            for edge in &edges {
                match commit_edges(db, std::slice::from_ref(edge)) {
                    Ok(inserted) => {
                        run.report.edges_inserted += inserted;
                        any = true;
                    }
                    Err(e) => run.bad_row(file, edge.3, &e.to_string())?,
                }
            }
            if any {
                run.committed();
            }
        }
    }
    Ok(())
}

/// The files one import reads. Any of them may be absent; at least one is
/// needed. They load in this order: nodes (CSV, then JSONL), edges (CSV, then
/// JSONL), rules. A JSONL path of `-` reads standard input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportInputs {
    pub nodes_csv: Option<PathBuf>,
    pub rels_csv: Option<PathBuf>,
    pub nodes_jsonl: Option<PathBuf>,
    pub edges_jsonl: Option<PathBuf>,
    pub rules_jsonl: Option<PathBuf>,
    /// One stream of `{"type": "node" | "edge" | "rule", …}` lines, which
    /// `export --format jsonl --single-file` writes.
    pub stream_jsonl: Option<PathBuf>,
}

impl ImportInputs {
    fn is_empty(&self) -> bool {
        self.nodes_csv.is_none()
            && self.rels_csv.is_none()
            && self.nodes_jsonl.is_none()
            && self.edges_jsonl.is_none()
            && self.rules_jsonl.is_none()
            && self.stream_jsonl.is_none()
    }

    fn csv_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.nodes_csv.iter().chain(self.rels_csv.iter())
    }

    fn jsonl_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.nodes_jsonl
            .iter()
            .chain(self.edges_jsonl.iter())
            .chain(self.rules_jsonl.iter())
            .chain(self.stream_jsonl.iter())
    }
}

fn is_stdin(path: &Path) -> bool {
    path.as_os_str() == "-"
}

// ── JSON lines ───────────────────────────────────────────────────────────

/// The five type tags of the typed JSON forms (`docs/site/http.md`).
const TYPE_TAGS: [&str; 5] = ["$date", "$ldt", "$dt", "$dur", "$binary"];

/// A JSON value as a stored [`Value`], keeping a float a float.
///
/// [`mushroomdb::json_to_value`] reads an integral number such as `3.0` as an
/// integer, which is right for hand-written JSON and wrong for the file an
/// export wrote: `Float(3.0)` must come back as a float. A JSON number with a
/// fraction or exponent is a float here, one without is an integer. A one-key
/// object whose key is a type tag is the typed value, read by the engine's own
/// reader; `null` is an absent value, and inside a list it drops the whole list,
/// as in `json_to_value`.
pub fn json_to_exact_value(v: serde_json::Value) -> Result<Option<Value>, String> {
    use serde_json::Value as Json;
    Ok(match v {
        Json::Null => None,
        Json::Bool(b) => Some(Value::Bool(b)),
        Json::Number(n) => Some(match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().ok_or("the number is out of range")?),
        }),
        Json::String(s) => Some(Value::Str(s)),
        Json::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.into_iter().enumerate() {
                match json_to_exact_value(item).map_err(|e| format!("item {i}: {e}"))? {
                    Some(v) => out.push(v),
                    None => return Ok(None),
                }
            }
            Some(Value::List(out))
        }
        Json::Object(obj) => {
            if obj.len() == 1 && obj.keys().all(|k| TYPE_TAGS.contains(&k.as_str())) {
                return mushroomdb::json_to_value(Json::Object(obj));
            }
            let mut map = BTreeMap::new();
            for (k, v) in obj {
                if let Some(val) = json_to_exact_value(v).map_err(|e| format!("key {k:?}: {e}"))? {
                    map.insert(k, val);
                }
            }
            Some(Value::Map(map))
        }
    })
}

/// The property of a `nodes.jsonl` line that holds the properties whose names
/// the line itself uses (`key`, `label`, and this one).
pub const JSONL_PROPS_FIELD: &str = "$props";

/// Parse one `nodes.jsonl` line: `{"key": …, "label": …, <properties>}`.
/// Properties whose name is `key`, `label` or `$props` are in a `$props`
/// object, which is how the export writes them.
fn parse_node_line(text: &str) -> Result<NodeRecord, String> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    parse_node_value(json, false)
}

/// [`parse_node_line`] for a line already parsed. In a stream (`typed`) the
/// `"type"` field is the line's discriminator, not a property (a property of
/// that name is in `$props`).
fn parse_node_value(json: serde_json::Value, typed: bool) -> Result<NodeRecord, String> {
    let serde_json::Value::Object(mut obj) = json else {
        return Err("a node line must be a JSON object".to_string());
    };
    if typed {
        obj.remove("type");
    }
    let mut text_field = |name: &str| match obj.remove(name) {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(s),
        Some(_) => Err(format!("`{name}` must be a non-empty string")),
        None => Err(format!("the line has no `{name}`")),
    };
    let key = text_field("key")?;
    // An empty `label` is a node with no label (QL-NM), which is how
    // `export` writes one.
    let label = match obj.remove("label") {
        Some(serde_json::Value::String(s)) => s,
        Some(_) => return Err("`label` must be a string".to_string()),
        None => return Err("the line has no `label`".to_string()),
    };
    let mut props: BTreeMap<String, Value> = BTreeMap::new();
    let reserved = match obj.remove(JSONL_PROPS_FIELD) {
        None => serde_json::Map::new(),
        Some(serde_json::Value::Object(m)) => m,
        Some(_) => return Err(format!("`{JSONL_PROPS_FIELD}` must be an object")),
    };
    for (name, value) in obj.into_iter().chain(reserved) {
        if let Some(v) =
            json_to_exact_value(value).map_err(|e| format!("property {name:?}: {e}"))?
        {
            props.insert(name, v);
        }
    }
    Ok(NodeRecord {
        label,
        key,
        props: props.into_iter().collect(),
        line: 0,
    })
}

/// One `edges.jsonl` line: `(edge_type, src, dst, derived)`.
fn parse_edge_line(text: &str) -> Result<(String, String, String, bool), String> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    parse_edge_value(&json)
}

fn parse_edge_value(json: &serde_json::Value) -> Result<(String, String, String, bool), String> {
    let serde_json::Value::Object(obj) = json else {
        return Err("an edge line must be a JSON object".to_string());
    };
    let field = |name: &str| match obj.get(name) {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(_) => Err(format!("`{name}` must be a non-empty string")),
        None => Err(format!("the line has no `{name}`")),
    };
    let derived = matches!(obj.get("derived"), Some(serde_json::Value::Bool(true)));
    Ok((field("edge_type")?, field("src")?, field("dst")?, derived))
}

/// A line source over a file, or standard input for `-`. Reports each non-blank
/// line with its 1-based number to `each`.
fn for_each_line(
    path: &Path,
    mut each: impl FnMut(u64, &str) -> Result<(), CliError>,
) -> Result<(), CliError> {
    use std::io::BufRead;
    let name = path.display().to_string();
    let mut reader: Box<dyn BufRead> = if is_stdin(path) {
        Box::new(std::io::BufReader::new(std::io::stdin().lock()))
    } else {
        let file = std::fs::File::open(path).map_err(|e| CliError(format!("{name}: {e}")))?;
        Box::new(std::io::BufReader::with_capacity(1 << 20, file))
    };
    let mut buf = String::new();
    let mut line = 0u64;
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(CliError(format!("{name}:{}: {e}", line + 1))),
        }
        line += 1;
        let text = buf.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            continue;
        }
        each(line, text)?;
    }
}

fn import_nodes_jsonl(db: &mut Db, path: &Path, run: &mut Run<'_>) -> Result<(), CliError> {
    let file = path.display().to_string();
    let mut records: Vec<NodeRecord> = Vec::new();
    let mut batch_keys: HashSet<String> = HashSet::new();
    for_each_line(path, |line, text| {
        let mut record = match parse_node_line(text) {
            Ok(r) => r,
            Err(reason) => return run.bad_row(&file, line, &reason),
        };
        record.line = line;
        if db.has_node(&record.key) || !batch_keys.insert(record.key.clone()) {
            let reason = format!("duplicate key `{}`", record.key);
            return run.bad_row(&file, line, &reason);
        }
        records.push(record);
        if records.len() >= run.opts.batch_size {
            flush_node_records(db, &mut records, &file, run)?;
            batch_keys.clear();
        }
        Ok(())
    })?;
    flush_node_records(db, &mut records, &file, run)
}

fn import_edges_jsonl(db: &mut Db, path: &Path, run: &mut Run<'_>) -> Result<(), CliError> {
    let file = path.display().to_string();
    let mut batch: Vec<(String, String, String, u64)> = Vec::new();
    for_each_line(path, |line, text| {
        let (edge_type, src, dst, derived) = match parse_edge_line(text) {
            Ok(parsed) => parsed,
            Err(reason) => return run.bad_row(&file, line, &reason),
        };
        if derived {
            run.report.derived_edges_skipped += 1;
            return Ok(());
        }
        let missing = [("src", &src), ("dst", &dst)]
            .into_iter()
            .find(|(_, key)| !db.has_node(key));
        if let Some((field, key)) = missing {
            let reason = format!("{field} `{key}` is not a node in the nodes file or the store");
            return run.bad_row(&file, line, &reason);
        }
        batch.push((edge_type, src, dst, line));
        if batch.len() >= run.opts.batch_size {
            flush_edges(db, &mut batch, &file, run)?;
        }
        Ok(())
    })?;
    flush_edges(db, &mut batch, &file, run)
}

/// Create the rules of a `rules.jsonl`, each in its own commit, after the
/// nodes and edges are in: the rule backfills once over all of them.
fn import_rules_jsonl(db: &mut Db, path: &Path, run: &mut Run<'_>) -> Result<(), CliError> {
    let file = path.display().to_string();
    for_each_line(path, |line, text| {
        let def: mushroomdb::RuleDef = match serde_json::from_str(text) {
            Ok(def) => def,
            Err(e) => return run.bad_row(&file, line, &format!("not a rule: {e}")),
        };
        match db.create_rule(def) {
            Ok(()) => {
                run.report.rules_created += 1;
                run.committed();
                Ok(())
            }
            Err(e) => run.bad_row(&file, line, &e.to_string()),
        }
    })
}

/// Import a single stream of typed lines (`export --format jsonl
/// --single-file`): `{"type":"node",…}`, `{"type":"edge",…}` and
/// `{"type":"rule","rule":{…}}`.
///
/// Lines are taken in order and nodes and edges are committed in batches as
/// they arrive. An edge must come after both its nodes: one whose node is not
/// in the store or earlier in the stream is an error at its line (a node still
/// waiting in the open batch counts, and is committed first). Rules are created
/// last, so each backfills once over everything.
fn import_stream_jsonl(db: &mut Db, path: &Path, run: &mut Run<'_>) -> Result<(), CliError> {
    let file = path.display().to_string();
    let mut records: Vec<NodeRecord> = Vec::new();
    let mut batch_keys: HashSet<String> = HashSet::new();
    let mut edges: Vec<(String, String, String, u64)> = Vec::new();
    let mut rules: Vec<(u64, mushroomdb::RuleDef)> = Vec::new();
    for_each_line(path, |line, text| {
        let json: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => return run.bad_row(&file, line, &format!("not JSON: {e}")),
        };
        let kind = json.get("type").and_then(serde_json::Value::as_str);
        match kind {
            Some("node") => {
                let mut record = match parse_node_value(json, true) {
                    Ok(r) => r,
                    Err(reason) => return run.bad_row(&file, line, &reason),
                };
                record.line = line;
                if db.has_node(&record.key) || !batch_keys.insert(record.key.clone()) {
                    let reason = format!("duplicate key `{}`", record.key);
                    return run.bad_row(&file, line, &reason);
                }
                records.push(record);
                if records.len() >= run.opts.batch_size {
                    flush_node_records(db, &mut records, &file, run)?;
                    batch_keys.clear();
                }
            }
            Some("edge") => {
                let (edge_type, src, dst, derived) = match parse_edge_value(&json) {
                    Ok(parsed) => parsed,
                    Err(reason) => return run.bad_row(&file, line, &reason),
                };
                if derived {
                    run.report.derived_edges_skipped += 1;
                    return Ok(());
                }
                if [&src, &dst].iter().any(|k| batch_keys.contains(*k)) {
                    flush_node_records(db, &mut records, &file, run)?;
                    batch_keys.clear();
                }
                let missing = [("src", &src), ("dst", &dst)]
                    .into_iter()
                    .find(|(_, key)| !db.has_node(key));
                if let Some((field, key)) = missing {
                    let reason = format!(
                        "{field} `{key}` is not a node earlier in the stream or in the store \
                         (an edge must come after its nodes)"
                    );
                    return run.bad_row(&file, line, &reason);
                }
                edges.push((edge_type, src, dst, line));
                if edges.len() >= run.opts.batch_size {
                    flush_edges(db, &mut edges, &file, run)?;
                }
            }
            Some("rule") => match json.get("rule").cloned().map(serde_json::from_value) {
                Some(Ok(def)) => rules.push((line, def)),
                Some(Err(e)) => return run.bad_row(&file, line, &format!("not a rule: {e}")),
                None => return run.bad_row(&file, line, "the line has no `rule`"),
            },
            Some(other) => {
                let reason = format!("unknown line type `{other}` (node, edge or rule)");
                return run.bad_row(&file, line, &reason);
            }
            None => return run.bad_row(&file, line, "the line has no string `type`"),
        }
        Ok(())
    })?;
    flush_node_records(db, &mut records, &file, run)?;
    flush_edges(db, &mut edges, &file, run)?;
    for (line, def) in rules {
        match db.create_rule(def) {
            Ok(()) => {
                run.report.rules_created += 1;
                run.committed();
            }
            Err(e) => run.bad_row(&file, line, &e.to_string())?,
        }
    }
    Ok(())
}

/// Import `nodes_csv` and then `rels_csv` into the store at `db_dir`
/// (created if absent). At least one file is required.
pub fn run_import(
    db_dir: &Path,
    nodes_csv: Option<&Path>,
    rels_csv: Option<&Path>,
    opts: &ImportOptions,
) -> Result<ImportReport, CliError> {
    run_import_with_progress(db_dir, nodes_csv, rels_csv, opts, &mut |_| {})
}

fn csv_inputs(nodes_csv: Option<&Path>, rels_csv: Option<&Path>) -> ImportInputs {
    ImportInputs {
        nodes_csv: nodes_csv.map(Path::to_path_buf),
        rels_csv: rels_csv.map(Path::to_path_buf),
        ..ImportInputs::default()
    }
}

/// [`run_import`], calling `on_batch` after every batch that committed.
pub fn run_import_with_progress(
    db_dir: &Path,
    nodes_csv: Option<&Path>,
    rels_csv: Option<&Path>,
    opts: &ImportOptions,
    on_batch: &mut dyn FnMut(&BatchProgress),
) -> Result<ImportReport, CliError> {
    run_import_inputs(db_dir, &csv_inputs(nodes_csv, rels_csv), opts, on_batch)
}

/// Import every file of `inputs` into the store at `db_dir` (created if
/// absent), then take one snapshot.
pub fn run_import_inputs(
    db_dir: &Path,
    inputs: &ImportInputs,
    opts: &ImportOptions,
    on_batch: &mut dyn FnMut(&BatchProgress),
) -> Result<ImportReport, CliError> {
    check_inputs(inputs, opts)?;
    let mut db = GraphDb::open(db_dir)?;
    let report = import_inputs_into(&mut db, inputs, opts, on_batch)?;
    // One snapshot, last. History-preserving, like every snapshot the engine
    // takes on its own: a bulk load must not erase how the store got here.
    // Nothing committed (a header-only file) means nothing to fold in.
    if report.batches > 0 {
        db.snapshot_with(crate::AUTOMATIC_SNAPSHOT)?;
    }
    Ok(report)
}

fn check_inputs(inputs: &ImportInputs, opts: &ImportOptions) -> Result<(), CliError> {
    if inputs.is_empty() {
        return Err(CliError(
            "import needs --nodes, --relationships, --nodes-jsonl, --edges-jsonl, --rules-jsonl or --jsonl"
                .into(),
        ));
    }
    if opts.batch_size == 0 {
        return Err(CliError("--batch-size must be at least 1".into()));
    }
    let files = inputs
        .csv_paths()
        .chain(inputs.jsonl_paths().filter(|p| !is_stdin(p)));
    for path in files {
        if !path.is_file() {
            return Err(CliError(format!("{}: no such file", path.display())));
        }
    }
    if inputs.jsonl_paths().filter(|p| is_stdin(p)).count() > 1 {
        return Err(CliError(
            "only one of the JSONL inputs can be - (standard input)".into(),
        ));
    }
    for path in inputs.csv_paths() {
        check_file_quoting(path, opts)?;
    }
    Ok(())
}

/// The import itself, on a store the caller holds open and takes the snapshot
/// of: [`run_import_with_progress`] is this plus the open and the snapshot.
/// Public so a test can read the store while the importing handle is live.
pub fn import_into(
    db: &mut GraphDb<RealFs>,
    nodes_csv: Option<&Path>,
    rels_csv: Option<&Path>,
    opts: &ImportOptions,
    on_batch: &mut dyn FnMut(&BatchProgress),
) -> Result<ImportReport, CliError> {
    import_inputs_into(db, &csv_inputs(nodes_csv, rels_csv), opts, on_batch)
}

/// [`import_into`] for every kind of input file.
pub fn import_inputs_into(
    db: &mut GraphDb<RealFs>,
    inputs: &ImportInputs,
    opts: &ImportOptions,
    on_batch: &mut dyn FnMut(&BatchProgress),
) -> Result<ImportReport, CliError> {
    check_inputs(inputs, opts)?;
    let mut run = Run {
        opts,
        report: ImportReport::default(),
        on_batch,
    };
    if let Some(path) = &inputs.nodes_csv {
        import_nodes(db, path, &mut run)?;
    }
    if let Some(path) = &inputs.nodes_jsonl {
        import_nodes_jsonl(db, path, &mut run)?;
    }
    if let Some(path) = &inputs.rels_csv {
        import_rels(db, path, &mut run)?;
    }
    if let Some(path) = &inputs.edges_jsonl {
        import_edges_jsonl(db, path, &mut run)?;
    }
    if let Some(path) = &inputs.stream_jsonl {
        import_stream_jsonl(db, path, &mut run)?;
    }
    if let Some(path) = &inputs.rules_jsonl {
        import_rules_jsonl(db, path, &mut run)?;
    }
    Ok(run.report)
}

/// The summary `mushroomdb import` prints.
pub fn format_import(report: &ImportReport) -> String {
    let mut out = format!(
        "imported {} nodes and {} edges in {} batches",
        report.nodes_inserted, report.edges_inserted, report.batches
    );
    if report.rules_created > 0 {
        out.push_str(&format!(", created {} rules", report.rules_created));
    }
    if report.derived_edges_skipped > 0 {
        out.push_str(&format!(
            ", left {} derived edges to their rules",
            report.derived_edges_skipped
        ));
    }
    if report.rows_skipped > 0 {
        out.push_str(&format!(", skipped {} bad rows", report.rows_skipped));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(cells: &[&str]) -> csv::StringRecord {
        csv::StringRecord::from(cells.to_vec())
    }

    #[test]
    fn csv_import_header_types_and_special_columns() {
        let s = parse_node_header(&rec(&[
            "personId:ID",
            ":LABEL",
            "name",
            "age:int",
            "weight:float",
            "active:boolean",
            "tags:string[]",
        ]))
        .unwrap();
        assert_eq!(s.key_field, "personId");
        assert_eq!((s.id_index, s.label_index), (0, 1));
        let tys: Vec<_> = s.props.iter().map(|p| (p.name.as_str(), p.ty)).collect();
        let t = |scalar, array| ColumnType { scalar, array };
        assert_eq!(
            tys,
            vec![
                ("name", t(ScalarType::Str, false)),
                ("age", t(ScalarType::Int, false)),
                ("weight", t(ScalarType::Float, false)),
                ("active", t(ScalarType::Bool, false)),
                ("tags", t(ScalarType::Str, true)),
            ]
        );
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn csv_import_unnamed_id_column_keys_as_id() {
        let s = parse_node_header(&rec(&[":ID", ":LABEL"])).unwrap();
        assert_eq!(s.key_field, "id");
    }

    #[test]
    fn csv_import_id_spaces_are_refused_clearly() {
        for header in [
            rec(&["personId:ID(People)", ":LABEL"]),
            rec(&[":ID(People)", ":LABEL"]),
        ] {
            let e = parse_node_header(&header).unwrap_err();
            assert!(
                e.contains("id spaces are not supported in mushroomdb 0.10"),
                "{e}"
            );
        }
        for header in [
            rec(&[":START_ID(People)", ":END_ID", ":TYPE"]),
            rec(&[":START_ID", ":END_ID(People)", ":TYPE"]),
        ] {
            let e = parse_rel_header(&header).unwrap_err();
            assert!(
                e.contains("id spaces are not supported in mushroomdb 0.10"),
                "{e}"
            );
        }
    }

    #[test]
    fn csv_import_missing_id_is_a_header_error() {
        let e = parse_node_header(&rec(&[":LABEL", "name"])).unwrap_err();
        assert!(e.contains(":ID"), "{e}");
        let e = parse_node_header(&rec(&[":ID", "name"])).unwrap_err();
        assert!(e.contains(":LABEL"), "{e}");
    }

    #[test]
    fn csv_import_unknown_type_suffix_warns_and_is_a_string() {
        let s = parse_node_header(&rec(&[":ID", ":LABEL", "born:uuid"])).unwrap();
        assert_eq!(s.props[0].ty.scalar, ScalarType::Str);
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("born:uuid"), "{:?}", s.warnings);
    }

    #[test]
    fn csv_import_duplicate_property_columns_are_refused() {
        let e = parse_node_header(&rec(&[":ID", ":LABEL", "a", "a:int"])).unwrap_err();
        assert!(e.contains("twice"), "{e}");
        let e = parse_node_header(&rec(&["id:ID", ":LABEL", "id"])).unwrap_err();
        assert!(e.contains("same name"), "{e}");
    }

    #[test]
    fn csv_import_relationship_header() {
        let s = parse_rel_header(&rec(&[":START_ID", ":END_ID", ":TYPE"])).unwrap();
        assert_eq!((s.start_index, s.end_index, s.type_index), (0, 1, 2));
        let e = parse_rel_header(&rec(&[":START_ID", ":TYPE"])).unwrap_err();
        assert!(e.contains(":END_ID"), "{e}");
        let e = parse_rel_header(&rec(&[":ID", ":START_ID", ":END_ID", ":TYPE"])).unwrap_err();
        assert!(e.contains("node column"), "{e}");
    }

    type ParsedRow = Result<(String, BTreeMap<String, Value>), String>;

    fn parse_one(csv_text: &str) -> Vec<ParsedRow> {
        let mut r = csv::ReaderBuilder::new().from_reader(csv_text.as_bytes());
        let schema = parse_node_header(&r.headers().unwrap().clone()).unwrap();
        r.records()
            .map(|rec| parse_node_row(&schema, &rec.unwrap(), ';'))
            .collect()
    }

    #[test]
    fn csv_import_quoted_fields_keep_embedded_delimiters_and_newlines() {
        let rows = parse_one("n:ID,:LABEL,name\n1,\"PERSON\",\"last, first\"\n2,P,\"a\nb\"\n");
        let (label, row) = rows[0].clone().unwrap();
        assert_eq!(label, "PERSON");
        assert_eq!(row["name"], Value::Str("last, first".into()));
        assert_eq!(
            rows[1].clone().unwrap().1["name"],
            Value::Str("a\nb".into())
        );
    }

    #[test]
    fn csv_import_multi_label_row_is_an_error_not_a_panic() {
        let rows = parse_one(":ID,:LABEL\n1,Person;Employee\n");
        let e = rows[0].clone().unwrap_err();
        assert!(e.contains("more than one label"), "{e}");
    }

    #[test]
    fn csv_import_empty_id_is_an_error_and_an_empty_label_is_no_label() {
        let rows = parse_one(":ID,:LABEL\n1,\n,P\n");
        assert_eq!(
            rows[0].clone().unwrap().0,
            "",
            "a node with no label (QL-NM)"
        );
        assert!(rows[1].clone().unwrap_err().contains(":ID"));
    }

    #[test]
    fn csv_import_values_are_typed_and_empty_cells_are_absent() {
        let rows = parse_one(
            ":ID,:LABEL,age:int,w:float,ok:boolean,tags:string[],note\n\
             a,P,42,1.5,TRUE,x;y;z,\n\
             b,P,,,,,\n",
        );
        let (_, a) = rows[0].clone().unwrap();
        assert_eq!(a["age"], Value::Int(42));
        assert_eq!(a["w"], Value::Float(1.5));
        assert_eq!(a["ok"], Value::Bool(true));
        assert_eq!(
            a["tags"],
            Value::List(
                vec!["x".into(), "y".into(), "z".into()]
                    .into_iter()
                    .map(Value::Str)
                    .collect()
            )
        );
        assert!(!a.contains_key("note"));
        let (_, b) = rows[1].clone().unwrap();
        assert_eq!(b.len(), 1, "only the key: {b:?}");
    }

    fn quoting(text: &str) -> Result<(), String> {
        check_quoting(text.as_bytes(), b',')
    }

    #[test]
    fn csv_import_quoting_check_accepts_rfc_4180() {
        quoting("a,b\n\"x,y\",\"say \"\"hi\"\"\"\n\"multi\nline\",z\r\n").unwrap();
        quoting("").unwrap();
        quoting("a,\"\"\n").unwrap();
    }

    #[test]
    fn csv_import_quoting_check_names_the_line() {
        let e = quoting("a,b\n1,\"never closed\n2,x\n").unwrap_err();
        assert!(e.starts_with("2:") && e.contains("never closed"), "{e}");
        let e = quoting("a,b\n1,ab\"c\n").unwrap_err();
        assert!(e.starts_with("2:") && e.contains("unquoted"), "{e}");
        let e = quoting("a\n\"x\"y\n").unwrap_err();
        assert!(e.starts_with("2:") && e.contains("closing quote"), "{e}");
        let e = quoting("a\n\"x\ny\"z\n").unwrap_err();
        assert!(
            e.starts_with("3:"),
            "line counts newlines inside a quoted field: {e}"
        );
    }

    #[test]
    fn csv_import_bad_typed_values_name_the_column() {
        let rows = parse_one(":ID,:LABEL,age:int,ok:boolean\n1,P,old,true\n2,P,3,maybe\n");
        let e = rows[0].clone().unwrap_err();
        assert!(e.contains("age") && e.contains("old"), "{e}");
        let e = rows[1].clone().unwrap_err();
        assert!(e.contains("ok") && e.contains("maybe"), "{e}");
    }

    #[test]
    fn jsonl_import_node_line_reads_tagged_forms_and_reserved_names() {
        let line = r#"{"key":"k","label":"L","born":{"$date":"2024-01-15"},"blob":{"$binary":"3q2+7w=="},"f":2.0,"$props":{"key":"x","label":1,"$props":true}}"#;
        let r = parse_node_line(line).unwrap();
        let props: BTreeMap<_, _> = r.props.into_iter().collect();
        assert_eq!((r.key.as_str(), r.label.as_str()), ("k", "L"));
        assert_eq!(props["born"], Value::Date(19_737));
        assert_eq!(props["blob"], Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        assert_eq!(props["f"], Value::Float(2.0));
        assert_eq!(props["key"], Value::Str("x".into()));
        assert_eq!(props["label"], Value::Int(1));
        assert_eq!(props["$props"], Value::Bool(true));
    }

    #[test]
    fn jsonl_import_node_line_refuses_bad_shapes() {
        for (line, why) in [
            ("[]", "must be a JSON object"),
            (r#"{"label":"L"}"#, "no `key`"),
            (r#"{"key":"","label":"L"}"#, "non-empty"),
            (r#"{"key":"k","label":"L","$props":3}"#, "must be an object"),
            (
                r#"{"key":"k","label":"L","d":{"$date":"nope"}}"#,
                "property \"d\"",
            ),
        ] {
            let e = parse_node_line(line).err().unwrap_or_default();
            assert!(e.contains(why), "{line}: {e}");
        }
    }

    #[test]
    fn csv_import_typed_columns_and_key_column() {
        let schema = parse_node_header(&rec(&[
            ":KEY",
            ":LABEL",
            "d:date",
            "t:datetime",
            "u:duration",
            "b:binary",
            "j:json",
            "a:b:string",
        ]))
        .unwrap();
        assert!(!schema.key_stored);
        let row = rec(&[
            "k",
            "L",
            "2024-01-15",
            "2024-01-15T12:34:56.789012+05:30",
            "P1Y2M3DT4H5M6.789S",
            "3q2+7w==",
            r#"[1.0,{"$date":"2024-01-15"}]"#,
            "x",
        ]);
        let (_, props) = parse_node_row(&schema, &row, ';').unwrap();
        assert!(!props.contains_key("id"));
        assert_eq!(props["d"], Value::Date(19_737));
        assert!(matches!(
            props["t"],
            Value::DateTime {
                offset_seconds: 19_800,
                ..
            }
        ));
        assert!(matches!(
            props["u"],
            Value::Duration {
                months: 14,
                days: 3,
                ..
            }
        ));
        assert_eq!(props["b"], Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        assert_eq!(
            props["j"],
            Value::List(vec![Value::Float(1.0), Value::Date(19_737)])
        );
        assert_eq!(props["a:b"], Value::Str("x".into()));
        let bad = rec(&["k", "L", "not a date", "", "", "", "", ""]);
        let e = parse_node_row(&schema, &bad, ';').unwrap_err();
        assert!(e.contains("column `d`"), "{e}");
        assert!(parse_node_header(&rec(&[":ID", ":KEY", ":LABEL"])).is_err());
    }
}
