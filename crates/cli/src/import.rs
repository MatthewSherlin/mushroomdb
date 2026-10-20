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
use std::path::Path;

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
    // `:ID(Space)` names an id space; every id here lives in one key space.
    let special = upper.split('(').next().unwrap_or("");
    match special {
        "ID" => return Ok(Column::Id(name.to_string())),
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
            Column::Label => cols.label.push(index),
            Column::StartId => cols.start.push(index),
            Column::EndId => cols.end.push(index),
            Column::RelType => cols.rel_type.push(index),
            Column::Ignore => {}
            Column::Prop(p) => cols.props.push(p),
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
    let id_indexes: Vec<usize> = cols.id.iter().map(|(i, _)| *i).collect();
    let id_index = exactly_one(&id_indexes, ":ID")?;
    let label_index = exactly_one(&cols.label, ":LABEL")?;
    let key_field = match cols.id[0].1.as_str() {
        "" => "id".to_string(),
        name => name.to_string(),
    };
    if cols.props.iter().any(|p| p.name == key_field) {
        return Err(format!(
            "property column `{key_field}` has the same name as the :ID column"
        ));
    }
    Ok(NodeSchema {
        key_field,
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
    if !cols.id.is_empty() || !cols.label.is_empty() {
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
    let label = &record[schema.label_index];
    if label.is_empty() {
        return Err("empty :LABEL".to_string());
    }
    if label.contains(array_delimiter) {
        return Err(format!(
            "`{label}` has more than one label; mushroomdb stores one label per node"
        ));
    }
    let mut row = BTreeMap::new();
    row.insert(schema.key_field.clone(), Value::Str(id.to_string()));
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
        let Some(Value::Str(key)) = row.get(&schema.key_field) else {
            unreachable!("parse_node_row stores the key as a string");
        };
        if db.has_node(key) || !batch_keys.insert(key.clone()) {
            let reason = format!("duplicate :ID `{key}`");
            run.bad_row(&file, line, &reason)?;
            continue;
        }
        batch.entry(label).or_default().push(row);
        buffered += 1;
        if buffered >= run.opts.batch_size {
            flush_nodes(db, &mut batch, &ingest_opts, run)?;
            batch_keys.clear();
            buffered = 0;
        }
    }
    flush_nodes(db, &mut batch, &ingest_opts, run)?;
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
        Err(_) => {
            // The engine rejects a batch whole. Name the row that did it:
            // a row that fails alone is the bad one, the rest are retried.
            for edge in &edges {
                match commit_edges(db, std::slice::from_ref(edge)) {
                    Ok(inserted) => {
                        run.report.edges_inserted += inserted;
                        run.committed();
                    }
                    Err(e) => run.bad_row(file, edge.3, &e.to_string())?,
                }
            }
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

/// [`run_import`], calling `on_batch` after every batch that committed.
pub fn run_import_with_progress(
    db_dir: &Path,
    nodes_csv: Option<&Path>,
    rels_csv: Option<&Path>,
    opts: &ImportOptions,
    on_batch: &mut dyn FnMut(&BatchProgress),
) -> Result<ImportReport, CliError> {
    if nodes_csv.is_none() && rels_csv.is_none() {
        return Err(CliError(
            "import needs --nodes <file>, --relationships <file>, or both".into(),
        ));
    }
    if opts.batch_size == 0 {
        return Err(CliError("--batch-size must be at least 1".into()));
    }
    for path in nodes_csv.iter().chain(rels_csv.iter()) {
        if !path.is_file() {
            return Err(CliError(format!("{}: no such file", path.display())));
        }
    }
    for path in nodes_csv.iter().chain(rels_csv.iter()) {
        check_file_quoting(path, opts)?;
    }
    let mut db = GraphDb::open(db_dir)?;
    let mut run = Run {
        opts,
        report: ImportReport::default(),
        on_batch,
    };
    if let Some(path) = nodes_csv {
        import_nodes(&mut db, path, &mut run)?;
    }
    if let Some(path) = rels_csv {
        import_rels(&mut db, path, &mut run)?;
    }
    // One snapshot, last. History-preserving, like every snapshot the engine
    // takes on its own: a bulk load must not erase how the store got here.
    db.snapshot_with(crate::AUTOMATIC_SNAPSHOT)?;
    Ok(run.report)
}

/// The summary `mushroomdb import` prints.
pub fn format_import(report: &ImportReport) -> String {
    let mut out = format!(
        "imported {} nodes and {} edges in {} batches",
        report.nodes_inserted, report.edges_inserted, report.batches
    );
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
        let s = parse_node_header(&rec(&[":ID(People)", ":LABEL"])).unwrap();
        assert_eq!(s.key_field, "id");
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
        let s = parse_node_header(&rec(&[":ID", ":LABEL", "born:date"])).unwrap();
        assert_eq!(s.props[0].ty.scalar, ScalarType::Str);
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("born:date"), "{:?}", s.warnings);
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
    fn csv_import_empty_label_and_empty_id_are_errors() {
        let rows = parse_one(":ID,:LABEL\n1,\n,P\n");
        assert!(rows[0].clone().unwrap_err().contains(":LABEL"));
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
}
