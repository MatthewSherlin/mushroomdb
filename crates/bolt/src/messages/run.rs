//! `RUN`, `PULL` and `DISCARD`: running a statement and streaming its rows.
//!
//! `RUN` carries `[query, parameters, extra]`. Where the statement runs depends
//! on the connection state:
//!
//! - **Auto-commit** (state `Ready`): a read runs on a lock-free
//!   [`ReaderSnapshot`](mushroomdb::ReaderSnapshot); a write goes through the
//!   write path the HTTP `/query` route uses (`query_write`, or
//!   `query_write_authz` for a role token), so it is one durable commit before
//!   the reply. A role token's reads run under its mask. Bookmarks in `extra`
//!   delay the statement until the store has reached them.
//! - **In a transaction** (state `InTx`): `Transaction::execute`, which buffers
//!   writes and reads its own writes. A role token's or `read` key's
//!   transaction reads through the role's mask on its `BEGIN` snapshot and
//!   refuses writes, as HTTP `/tx` does (see `messages::tx::ScopedTransaction`).
//!
//! The reply to `RUN` is `SUCCESS {fields, t_first}`; rows stay in a cursor
//! until `PULL {n, qid}` sends up to `n` of them as `RECORD`s (`n = -1` is all)
//! or `DISCARD {n, qid}` drops them. A `PULL` that leaves rows behind ends in
//! `SUCCESS {has_more: true}`; the last one carries the summary (`type`,
//! `t_last`, and in auto-commit the `bookmark`).
//!
//! A bare node, relationship or path variable goes out as the Bolt Node,
//! Relationship or Path structure (see [`graph`](super::graph)): every
//! statement runs inside [`mushroomdb::with_entity_detail`], so the result
//! carries the element behind each such cell. A write's closing `SUCCESS`
//! carries its counters as `stats`.
//!
//! Rows are checked before they are sent. PackStream writes a string, byte
//! array, list or map length as 32 bits and a structure's field count as 16, so
//! a value over 4 GiB or a structure with more than 65535 fields would
//! truncate into a frame that decodes as something else. A row holding one
//! fails the `PULL` with a `FAILURE`; nothing from that `PULL` is sent.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mushroomdb::{
    explain_plan_value, is_explain_query, is_schema_query, is_write_query, with_entity_detail,
    GraphError, ResultSet, SharedDb, Transaction, Value,
};

use super::graph;
use super::values::{
    self, TAG_DATE, TAG_DATE_TIME, TAG_DURATION, TAG_LEGACY_DATE_TIME, TAG_LOCAL_DATE_TIME,
};
use super::{check_depth, MAX_PACK_LEN, TAG_RECORD};
use crate::connection::{OpenTransaction, ResultCursor};
use crate::error::BoltError;
use crate::messages::hello::Identity;
use crate::messages::tx::{BeginRequest, ReadOnlyTransaction, ScopedTransaction};
use crate::packstream::{PackStruct, PackValue, MAX_DEPTH};

/// How long a statement waits for the store to reach a bookmark.
pub const DEFAULT_BOOKMARK_TIMEOUT: Duration = Duration::from_secs(5);

const BOOKMARK_PREFIX: &str = "mushroomdb:commit:";
const BOOKMARK_POLL: Duration = Duration::from_millis(10);

/// The text of a bookmark for commit `seq`.
pub fn format_bookmark(seq: u64) -> String {
    format!("{BOOKMARK_PREFIX}{seq}")
}

/// The commit a bookmark names.
pub fn parse_bookmark(text: &str) -> Result<u64, BoltError> {
    text.strip_prefix(BOOKMARK_PREFIX)
        .and_then(|rest| rest.parse::<u64>().ok())
        .ok_or_else(|| BoltError::Invalid(format!("unrecognised bookmark {text:?}")))
}

pub(crate) fn map_get<'a>(map: &'a [(String, PackValue)], key: &str) -> Option<&'a PackValue> {
    map.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// A decoded `RUN`.
#[derive(Clone, PartialEq)]
pub struct RunRequest {
    pub query: String,
    parameters: Vec<(String, PackValue)>,
    /// The `db` field of `extra`; `None` when absent or null.
    pub database: Option<String>,
    pub bookmarks: Vec<u64>,
}

impl std::fmt::Debug for RunRequest {
    // Parameters can hold secrets and the query can embed them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunRequest").finish_non_exhaustive()
    }
}

impl RunRequest {
    /// Reads the fields of a `RUN` structure: a string and two maps. A wrong
    /// shape is a protocol violation; the caller decides.
    pub fn from_fields(fields: Vec<PackValue>) -> Result<Self, BoltError> {
        let mut fields = fields.into_iter();
        let (
            Some(PackValue::String(query)),
            Some(PackValue::Map(parameters)),
            Some(PackValue::Map(extra)),
            None,
        ) = (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(BoltError::Invalid(
                "RUN takes a query string, a parameter map and an extra map".into(),
            ));
        };
        let database = match map_get(&extra, "db") {
            None | Some(PackValue::Null) => None,
            Some(PackValue::String(name)) => Some(name.clone()),
            Some(_) => return Err(BoltError::Invalid("RUN db must be a string".into())),
        };
        let bookmarks = match map_get(&extra, "bookmarks") {
            None | Some(PackValue::Null) => Vec::new(),
            Some(PackValue::List(items)) => items
                .iter()
                .map(|item| match item {
                    PackValue::String(text) => parse_bookmark(text),
                    _ => Err(BoltError::Invalid("a bookmark must be a string".into())),
                })
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(BoltError::Invalid(
                    "RUN bookmarks must be a list of strings".into(),
                ))
            }
        };
        Ok(Self {
            query,
            parameters,
            database,
            bookmarks,
        })
    }

    /// The parameters as engine values, read as the negotiated `version`
    /// spells them. A Bolt null is `Value::Null`, so `$p IS NULL` is true for
    /// it; it is not left out, as the HTTP route leaves a JSON null out. A
    /// temporal structure becomes the engine's temporal value (see
    /// [`values`](super::values)); any other structure is refused as a client
    /// error.
    pub fn params(&self, version: (u8, u8)) -> Result<BTreeMap<String, Value>, BoltError> {
        let mut out = BTreeMap::new();
        for (name, value) in &self.parameters {
            out.insert(name.clone(), param_value(name, value, version, 0)?);
        }
        Ok(out)
    }
}

fn param_value(
    name: &str,
    value: &PackValue,
    version: (u8, u8),
    depth: usize,
) -> Result<Value, BoltError> {
    if depth > MAX_DEPTH {
        return Err(BoltError::Invalid(format!(
            "parameter {name:?} is nested too deeply"
        )));
    }
    Ok(match value {
        PackValue::Null => Value::Null,
        PackValue::Bool(b) => Value::Bool(*b),
        PackValue::Int(i) => Value::Int(*i),
        PackValue::Float(f) => Value::Float(*f),
        PackValue::String(s) => Value::Str(s.clone()),
        PackValue::Bytes(b) => Value::Binary(b.clone()),
        PackValue::List(items) => Value::List(
            items
                .iter()
                .map(|item| param_value(name, item, version, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        PackValue::Map(entries) => Value::Map(
            entries
                .iter()
                .map(|(k, v)| Ok((k.clone(), param_value(name, v, version, depth + 1)?)))
                .collect::<Result<_, BoltError>>()?,
        ),
        PackValue::Struct(s) => values::struct_param(name, s, version)?,
    })
}

/// A decoded `PULL` or `DISCARD`: the same two fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamRequest {
    /// Rows wanted; `None` is all of them (`n = -1`).
    pub limit: Option<usize>,
    /// The result addressed; `-1` is the latest.
    pub qid: i64,
}

impl StreamRequest {
    /// Reads the fields of a `PULL` or `DISCARD`: one map holding `n` and
    /// optionally `qid`. A wrong shape is a protocol violation; an `n` of zero
    /// or below -1 is a request fault, reported by [`limit_error`].
    pub fn from_fields(fields: Vec<PackValue>) -> Result<Result<Self, BoltError>, BoltError> {
        let [PackValue::Map(extra)] = fields.as_slice() else {
            return Err(BoltError::Invalid(
                "PULL and DISCARD take exactly one map field".into(),
            ));
        };
        let Some(PackValue::Int(n)) = map_get(extra, "n") else {
            return Err(BoltError::Invalid(
                "PULL and DISCARD need an integer n".into(),
            ));
        };
        let qid = match map_get(extra, "qid") {
            None | Some(PackValue::Null) => -1,
            Some(PackValue::Int(qid)) => *qid,
            Some(_) => return Err(BoltError::Invalid("qid must be an integer".into())),
        };
        let limit = match *n {
            -1 => Ok(None),
            n if n > 0 => Ok(Some(usize::try_from(n).unwrap_or(usize::MAX))),
            n => Err(BoltError::Invalid(format!(
                "n must be -1 or positive, got {n}"
            ))),
        };
        Ok(limit.map(|limit| Self { limit, qid }))
    }
}

/// The refusal for a role token that writes in an explicit transaction, word
/// for word what HTTP `/tx/{id}/run` gives it: the transaction engine has no
/// role-scoped write path (ledger row 140).
pub(crate) fn tx_role_write_refusal() -> BoltError {
    BoltError::Forbidden("role-bound token: writes are not permitted inside a transaction".into())
}

/// The refusal for a `read` key that runs a statement that writes, the text
/// HTTP gives it.
pub(crate) fn read_key_refusal() -> BoltError {
    BoltError::Forbidden("read-tier key: writes are not permitted".into())
}

/// An error from a role-scoped statement: a role the store no longer defines
/// is a failed login, as it is over HTTP (401); anything else maps as usual.
pub(crate) fn role_mask_error(err: GraphError) -> BoltError {
    match err {
        GraphError::KeyNotFound { key } if key.starts_with("role:") => BoltError::Unauthorized,
        other => BoltError::from(other),
    }
}

/// What `RUN` needs from the store.
#[derive(Clone)]
pub struct QueryService {
    db: SharedDb,
    bookmark_timeout: Duration,
    /// The negotiated Bolt version, which picks how temporal parameters are
    /// read.
    version: (u8, u8),
    /// Set when the client's `RESET` arrives while a request is running; a
    /// bookmark wait ends when it is set.
    interrupt: Arc<AtomicBool>,
}

impl std::fmt::Debug for QueryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryService").finish_non_exhaustive()
    }
}

impl QueryService {
    /// The service holds no database check on purpose: which databases a
    /// connection may name is decided by its `ConnectionConfig::routing`, in
    /// `Connection`, the same config that answers `ROUTE`. Nothing outside the
    /// crate can build a service, so nothing outside can supply a permissive
    /// check.
    pub(crate) fn new(db: SharedDb) -> Self {
        Self {
            db,
            bookmark_timeout: DEFAULT_BOOKMARK_TIMEOUT,
            version: (5, 0),
            interrupt: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Makes bookmark waits end when `flag` is set (the connection's `RESET`
    /// flag).
    pub(crate) fn with_interrupt(mut self, flag: Arc<AtomicBool>) -> Self {
        self.interrupt = flag;
        self
    }

    /// Reads parameters as Bolt `version` spells them.
    pub(crate) fn with_version(mut self, version: (u8, u8)) -> Self {
        self.version = version;
        self
    }

    pub(crate) fn with_bookmark_timeout(mut self, timeout: Duration) -> Self {
        self.bookmark_timeout = timeout;
        self
    }

    /// Runs `request` outside a transaction. A full-token read goes through the
    /// same engine path as the HTTP `/query` route; a role token's read runs
    /// on a snapshot under the role's mask. Blocks: a write waits for its
    /// fsync, and a bookmark wait polls.
    pub fn run_auto_commit(
        &self,
        identity: &Identity,
        request: &RunRequest,
    ) -> Result<Box<dyn ResultCursor>, BoltError> {
        with_entity_detail(|| self.run_auto_commit_detailed(identity, request))
    }

    fn run_auto_commit_detailed(
        &self,
        identity: &Identity,
        request: &RunRequest,
    ) -> Result<Box<dyn ResultCursor>, BoltError> {
        let params = request.params(self.version)?;
        let is_write = is_write_query(&request.query).map_err(BoltError::SyntaxError)?;
        self.wait_for_bookmarks(&request.bookmarks)?;
        if is_write && matches!(identity, Identity::ReadOnlyRole(_)) {
            return Err(read_key_refusal());
        }
        let (result, seq) = if is_write {
            let result = match identity {
                Identity::Full | Identity::Write => {
                    self.db.write().query_write(&request.query, &params)
                }
                Identity::Role(role) => {
                    self.db
                        .write()
                        .query_write_authz(role, &request.query, &params)
                }
                Identity::ReadOnlyRole(_) => unreachable!("refused above"),
            };
            (result.map_err(role_mask_error), self.db.reader().version)
        } else {
            match identity {
                // The read the HTTP route makes for a full token (UNION
                // included). TX-5 moves HTTP and Bolt to the lock-free reader
                // together; until its parser matches, Bolt follows HTTP.
                Identity::Full | Identity::Write => {
                    let result = self.db.read().query(&request.query, &params);
                    (result.map_err(BoltError::from), self.db.reader().version)
                }
                Identity::Role(role) | Identity::ReadOnlyRole(role) => {
                    let snapshot = self.db.reader();
                    let result = snapshot
                        .mask_for_role(role)
                        .and_then(|mask| snapshot.query_masked(&request.query, &params, &mask))
                        .map_err(role_mask_error);
                    (result, snapshot.version)
                }
            }
        };
        let result = result?;
        // A statement that changes the schema reports query type `s`.
        let schema = is_write && is_schema_query(&request.query).unwrap_or(false);
        Ok(Box::new(
            RowCursor::for_statement(&request.query, result, is_write, Some(format_bookmark(seq)))
                .with_schema(schema),
        ))
    }

    /// Runs `request` inside `tx`. A scoped identity's `tx` is the
    /// [`ScopedTransaction`] [`begin`](Self::begin) gave it, which applies the
    /// role. A scoped identity holding any other transaction (one handed to
    /// [`Connection::enter_tx`](crate::connection::Connection::enter_tx)
    /// directly) is refused: `Transaction::execute` applies no mask.
    pub fn run_in_tx(
        &self,
        identity: &Identity,
        tx: &mut dyn OpenTransaction,
        request: &RunRequest,
    ) -> Result<Box<dyn ResultCursor>, BoltError> {
        let scoped = matches!(identity, Identity::Role(_) | Identity::ReadOnlyRole(_));
        if scoped && !tx.is_scoped() {
            return Err(BoltError::Forbidden(
                "role-bound tokens cannot run statements in an unscoped transaction".into(),
            ));
        }
        let params = request.params(self.version)?;
        with_entity_detail(|| tx.execute(&request.query, &params))
    }

    /// Opens a transaction: waits for the request's bookmarks, then freezes
    /// the snapshot every statement of the transaction reads. Blocks while it
    /// waits. A `mode: "r"` request gets a transaction that refuses writes.
    ///
    /// A role token or a `read` key gets a [`ScopedTransaction`], as HTTP
    /// `/tx/begin` gives it a scoped body: the role's mask is resolved here, on
    /// the snapshot every statement reads, so a role the store no longer
    /// defines is refused at `BEGIN`. It is read-only whatever `mode` says.
    pub fn begin(
        &self,
        identity: &Identity,
        request: &BeginRequest,
    ) -> Result<Box<dyn OpenTransaction>, BoltError> {
        self.wait_for_bookmarks(&request.bookmarks)?;
        let role = match identity {
            Identity::Full | Identity::Write => None,
            Identity::Role(role) => Some((role, false)),
            Identity::ReadOnlyRole(role) => Some((role, true)),
        };
        if let Some((role, read_tier_key)) = role {
            let snapshot = self.db.reader();
            let mask = snapshot.mask_for_role(role).map_err(role_mask_error)?;
            return Ok(Box::new(ScopedTransaction::new(
                snapshot,
                mask,
                read_tier_key,
            )));
        }
        let tx: Box<dyn OpenTransaction> = Box::new(self.db.begin_transaction());
        Ok(if request.read_only {
            Box::new(ReadOnlyTransaction::new(tx))
        } else {
            tx
        })
    }

    /// Waits until the store has reached the highest of `bookmarks`: polls
    /// every 10 ms, up to the bookmark timeout, and gives up early when the
    /// client's `RESET` arrives.
    pub(crate) fn wait_for_bookmarks(&self, bookmarks: &[u64]) -> Result<(), BoltError> {
        let Some(wanted) = bookmarks.iter().copied().max() else {
            return Ok(());
        };
        let deadline = Instant::now() + self.bookmark_timeout;
        loop {
            if self.interrupt.load(Ordering::SeqCst) {
                return Err(BoltError::Unavailable(
                    "the wait for a bookmark was interrupted by RESET".into(),
                ));
            }
            if self.db.reader().version >= wanted {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BoltError::Unavailable(format!(
                    "the store did not reach bookmark {} within {:?}",
                    format_bookmark(wanted),
                    self.bookmark_timeout
                )));
            }
            std::thread::sleep(BOOKMARK_POLL);
        }
    }
}

impl OpenTransaction for Transaction {
    fn rollback(self: Box<Self>) {
        (*self).rollback();
    }

    fn commit(self: Box<Self>) -> Result<u64, BoltError> {
        Transaction::commit(*self).map_err(BoltError::from)
    }

    fn execute(
        &mut self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<Box<dyn ResultCursor>, BoltError> {
        let is_write = is_write_query(cypher).map_err(BoltError::SyntaxError)?;
        let result = Transaction::execute(self, cypher, params)?;
        Ok(Box::new(RowCursor::for_statement(
            cypher, result, is_write, None,
        )))
    }
}

/// The rows of one statement, held until they are pulled.
pub struct RowCursor {
    fields: Vec<String>,
    result: ResultSet,
    position: usize,
    is_write: bool,
    bookmark: Option<String>,
    opened: Instant,
    max_len: u64,
    /// The statement was an auto-commit write, already durable when its rows
    /// are pulled.
    committed: bool,
    /// The statement changed the schema: its query type is `s`, not `w`.
    schema: bool,
    /// For an `EXPLAIN`: the plan the closing `SUCCESS` carries as `plan`.
    plan: Option<PackValue>,
}

impl RowCursor {
    pub fn new(result: ResultSet, is_write: bool, bookmark: Option<String>) -> Self {
        let committed = is_write && bookmark.is_some();
        Self {
            fields: result.columns().to_vec(),
            result,
            position: 0,
            is_write,
            bookmark,
            opened: Instant::now(),
            max_len: MAX_PACK_LEN,
            committed,
            schema: false,
            plan: None,
        }
    }

    /// The cursor for `cypher`'s `result`. An `EXPLAIN` has no records: its
    /// plan goes in the closing `SUCCESS` as `plan` (`operatorType`, `args`,
    /// `identifiers`, `children`), where a driver reads `summary.plan`.
    pub fn for_statement(
        cypher: &str,
        result: ResultSet,
        is_write: bool,
        bookmark: Option<String>,
    ) -> Self {
        let plan = is_explain_query(cypher)
            .unwrap_or(false)
            .then(|| explain_plan_value(&result))
            .flatten()
            .and_then(|plan| pack_value(&plan, (5, 0), 0).ok());
        match plan {
            Some(plan) => {
                let mut cursor = Self::new(ResultSet::new(Vec::new()), false, bookmark);
                cursor.plan = Some(plan);
                cursor
            }
            None => Self::new(result, is_write, bookmark),
        }
    }

    /// Marks a statement that changed the schema, so its summary's query
    /// type is `s`.
    pub fn with_schema(mut self, schema: bool) -> Self {
        self.schema = schema;
        self
    }

    /// Lowers the size limit a value is checked against. The real limit is
    /// 4 GiB; a test uses a small one so it need not allocate that much.
    #[cfg(test)]
    pub(crate) fn with_max_len(mut self, max_len: u64) -> Self {
        self.max_len = max_len;
        self
    }

    /// An auto-commit write's rows are checked only after it ran, so a row the
    /// wire cannot carry fails a statement that has committed. Say so, or the
    /// client reads the failure as a write that did not happen.
    fn committed_note(&self, err: BoltError) -> BoltError {
        match err {
            BoltError::TooLarge(message) if self.committed => BoltError::TooLarge(format!(
                "{message}. The statement had already committed; only the returned rows could not be sent"
            )),
            other => other,
        }
    }

    fn end_of(&self, limit: Option<usize>) -> usize {
        match limit {
            Some(n) => self.position.saturating_add(n).min(self.result.len()),
            None => self.result.len(),
        }
    }
}

impl ResultCursor for RowCursor {
    fn fields(&self) -> &[String] {
        &self.fields
    }

    fn take(
        &mut self,
        limit: Option<usize>,
        version: (u8, u8),
    ) -> Result<Vec<Vec<PackValue>>, BoltError> {
        let end = self.end_of(limit);
        let mut rows = Vec::with_capacity(end - self.position);
        for index in self.position..end {
            let row = self
                .result
                .row(index)
                .iter()
                .enumerate()
                .map(|(col, cell)| {
                    graph::pack_cell(self.result.entity(index, col), cell.as_ref(), version, 0)
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| self.committed_note(err))?;
            check_row(&row, self.max_len).map_err(|err| self.committed_note(err))?;
            rows.push(row);
        }
        // Advance only when the whole batch is sendable: a refused row leaves
        // the cursor where it was, and the connection drops it.
        self.position = end;
        Ok(rows)
    }

    fn discard(&mut self, limit: Option<usize>) {
        self.position = self.end_of(limit);
    }

    fn has_more(&self) -> bool {
        self.position < self.result.len()
    }

    fn summary(&self) -> Vec<(String, PackValue)> {
        let mut meta = vec![
            (
                "type".to_string(),
                PackValue::String(
                    match (self.schema, self.is_write) {
                        (true, _) => "s",
                        (false, true) => "w",
                        (false, false) => "r",
                    }
                    .into(),
                ),
            ),
            (
                "t_last".to_string(),
                PackValue::Int(
                    i64::try_from(self.opened.elapsed().as_millis()).unwrap_or(i64::MAX),
                ),
            ),
        ];
        if let Some(stats) = self.result.stats().and_then(graph::stats_map) {
            meta.push(("stats".to_string(), stats));
        }
        if let Some(plan) = &self.plan {
            meta.push(("plan".to_string(), plan.clone()));
        }
        if let Some(bookmark) = &self.bookmark {
            meta.push(("bookmark".to_string(), PackValue::String(bookmark.clone())));
        }
        meta
    }
}

/// Refuses a row PackStream could not write without truncating: one value or
/// structure past the size headers, or a nesting deeper than a client decodes.
pub fn check_row(row: &[PackValue], max_len: u64) -> Result<(), BoltError> {
    if row.len() as u64 > max_len {
        return Err(BoltError::TooLarge(
            "result row cannot be sent over Bolt: it has more fields than PackStream can count"
                .into(),
        ));
    }
    // The row travels as the one field of a RECORD structure, a level down.
    row.iter().try_for_each(|v| check_depth(v, max_len, 1))
}

const MICROS_PER_SECOND: i64 = 1_000_000;
const NANOS_PER_SECOND: i64 = 1_000_000_000;

fn int(n: impl Into<i64>) -> PackValue {
    PackValue::Int(n.into())
}

fn structure(tag: u8, fields: Vec<PackValue>) -> PackValue {
    PackValue::Struct(PackStruct::new(tag, fields))
}

/// An engine value as PackStream. `version` picks the `DateTime` form: from
/// Bolt 5.0 it is UTC-based (`0x49`), before that it counts local seconds
/// (`0x46`). The structures' field orders are listed in
/// [`values`](super::values); microseconds go out as `micros × 1000`
/// nanoseconds.
pub fn pack_value(value: &Value, version: (u8, u8), depth: usize) -> Result<PackValue, BoltError> {
    if depth > MAX_DEPTH {
        return Err(BoltError::TooLarge(
            "result value cannot be sent over Bolt: nesting exceeds the PackStream depth limit"
                .into(),
        ));
    }
    Ok(match value {
        Value::Int(i) => PackValue::Int(*i),
        Value::Float(f) => PackValue::Float(*f),
        Value::Str(s) => PackValue::String(s.clone()),
        Value::Bool(b) => PackValue::Bool(*b),
        Value::List(items) => PackValue::List(
            items
                .iter()
                .map(|v| pack_value(v, version, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        Value::Map(entries) => PackValue::Map(
            entries
                .iter()
                .map(|(k, v)| Ok((k.clone(), pack_value(v, version, depth + 1)?)))
                .collect::<Result<_, BoltError>>()?,
        ),
        Value::Binary(bytes) => PackValue::Bytes(bytes.clone()),
        // A null element of a query-built list or map (never stored).
        Value::Null => PackValue::Null,
        Value::Date(days) => structure(TAG_DATE, vec![int(*days)]),
        Value::LocalDateTime(micros) => structure(
            TAG_LOCAL_DATE_TIME,
            vec![
                int(micros.div_euclid(MICROS_PER_SECOND)),
                int(micros.rem_euclid(MICROS_PER_SECOND) * 1000),
            ],
        ),
        Value::DateTime {
            micros_utc,
            offset_seconds,
        } => {
            let seconds = micros_utc.div_euclid(MICROS_PER_SECOND);
            let nanos = micros_utc.rem_euclid(MICROS_PER_SECOND) * 1000;
            if values::uses_utc_date_time(version) {
                structure(
                    TAG_DATE_TIME,
                    vec![int(seconds), int(nanos), int(*offset_seconds)],
                )
            } else {
                structure(
                    TAG_LEGACY_DATE_TIME,
                    vec![
                        int(seconds + i64::from(*offset_seconds)),
                        int(nanos),
                        int(*offset_seconds),
                    ],
                )
            }
        }
        Value::Duration {
            months,
            days,
            nanos,
        } => structure(
            TAG_DURATION,
            vec![
                int(*months),
                int(*days),
                int(nanos.div_euclid(NANOS_PER_SECOND)),
                int(nanos.rem_euclid(NANOS_PER_SECOND)),
            ],
        ),
    })
}

/// A `RECORD` message's structure.
pub fn record_struct(row: Vec<PackValue>) -> PackValue {
    structure(TAG_RECORD, vec![PackValue::List(row)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_fields(query: &str, extra: Vec<(String, PackValue)>) -> Vec<PackValue> {
        vec![
            PackValue::String(query.into()),
            PackValue::Map(vec![]),
            PackValue::Map(extra),
        ]
    }

    #[test]
    fn bookmarks_round_trip_and_reject_foreign_text() {
        assert_eq!(parse_bookmark(&format_bookmark(42)), Ok(42));
        assert!(parse_bookmark("mushroomdb:commit:x").is_err());
        assert!(parse_bookmark("other:commit:1").is_err());
        assert!(parse_bookmark("").is_err());
    }

    #[test]
    fn run_request_reads_db_and_bookmarks() {
        let extra = vec![
            ("db".to_string(), PackValue::String("mydb".into())),
            (
                "bookmarks".to_string(),
                PackValue::List(vec![
                    PackValue::String(format_bookmark(3)),
                    PackValue::String(format_bookmark(9)),
                ]),
            ),
        ];
        let req = RunRequest::from_fields(run_fields("RETURN 1", extra)).unwrap();
        assert_eq!(req.database.as_deref(), Some("mydb"));
        assert_eq!(req.bookmarks, vec![3, 9]);
        let plain = RunRequest::from_fields(run_fields("RETURN 1", vec![])).unwrap();
        assert_eq!(plain.database, None);
    }

    #[test]
    fn run_request_shape_errors() {
        assert!(RunRequest::from_fields(vec![]).is_err());
        assert!(RunRequest::from_fields(vec![PackValue::Null; 3]).is_err());
        let mut four = run_fields("RETURN 1", vec![]);
        four.push(PackValue::Null);
        assert!(RunRequest::from_fields(four).is_err());
    }

    #[test]
    fn parameters_never_leak_into_debug() {
        let secret = RunRequest::from_fields(vec![
            PackValue::String("RETURN $x".into()),
            PackValue::Map(vec![("x".into(), PackValue::String("secret-param".into()))]),
            PackValue::Map(vec![]),
        ])
        .unwrap();
        assert!(!format!("{secret:?}").contains("secret-param"));
        assert_eq!(
            secret.params((5, 0)).unwrap().get("x"),
            Some(&Value::Str("secret-param".into()))
        );
    }

    #[test]
    fn stream_request_limits() {
        let fields = |n: i64| vec![PackValue::Map(vec![("n".into(), PackValue::Int(n))])];
        assert_eq!(
            StreamRequest::from_fields(fields(-1)).unwrap().unwrap(),
            StreamRequest {
                limit: None,
                qid: -1
            }
        );
        assert_eq!(
            StreamRequest::from_fields(fields(100))
                .unwrap()
                .unwrap()
                .limit,
            Some(100)
        );
        assert!(StreamRequest::from_fields(fields(0)).unwrap().is_err());
        assert!(StreamRequest::from_fields(fields(-2)).unwrap().is_err());
        assert!(StreamRequest::from_fields(vec![]).is_err());
        assert!(StreamRequest::from_fields(vec![PackValue::Map(vec![])]).is_err());
    }

    #[test]
    fn a_null_element_of_a_result_list_packs_as_null() {
        let list = Value::List(vec![Value::Int(1), Value::Null]);
        assert_eq!(
            pack_value(&list, (5, 0), 0).unwrap(),
            PackValue::List(vec![PackValue::Int(1), PackValue::Null])
        );
    }

    #[test]
    fn temporal_values_use_the_version_forms() {
        let dt = Value::DateTime {
            micros_utc: 1_500_000,
            offset_seconds: 3600,
        };
        let v5 = pack_value(&dt, (5, 0), 0).unwrap();
        assert_eq!(
            v5,
            structure(0x49, vec![int(1), int(500_000_000), int(3600)])
        );
        let v44 = pack_value(&dt, (4, 4), 0).unwrap();
        assert_eq!(
            v44,
            structure(0x46, vec![int(3601), int(500_000_000), int(3600)])
        );
        let before_epoch = pack_value(&Value::LocalDateTime(-1), (5, 0), 0).unwrap();
        assert_eq!(
            before_epoch,
            structure(0x64, vec![int(-1), int(999_999_000)])
        );
        assert_eq!(
            pack_value(&Value::Date(19000), (5, 0), 0).unwrap(),
            structure(0x44, vec![int(19000)])
        );
        assert_eq!(
            pack_value(
                &Value::Duration {
                    months: 1,
                    days: 2,
                    nanos: 3_000_000_001
                },
                (5, 0),
                0
            )
            .unwrap(),
            structure(0x45, vec![int(1), int(2), int(3), int(1)])
        );
    }

    #[test]
    fn a_struct_over_65535_fields_in_a_row_is_refused() {
        let wide = PackValue::Struct(PackStruct::new(
            0x44,
            vec![PackValue::Null; super::super::MAX_STRUCT_FIELDS + 1],
        ));
        assert!(matches!(
            check_row(&[wide], MAX_PACK_LEN),
            Err(BoltError::TooLarge(_))
        ));
        let ok = PackValue::Struct(PackStruct::new(
            0x44,
            vec![PackValue::Null; super::super::MAX_STRUCT_FIELDS],
        ));
        assert!(check_row(&[ok], MAX_PACK_LEN).is_ok());
    }

    #[test]
    fn a_value_over_the_size_limit_is_refused_wherever_it_sits() {
        // The real limit is 4 GiB; the limit is a parameter so the test need
        // not allocate it.
        let long = PackValue::String("x".repeat(9));
        assert!(check_row(std::slice::from_ref(&long), 8).is_err());
        assert!(check_row(std::slice::from_ref(&long), 9).is_ok());
        let nested = PackValue::List(vec![PackValue::Map(vec![("k".into(), long)])]);
        assert!(check_row(&[nested], 8).is_err());
        assert!(check_row(&[PackValue::Bytes(vec![0; 9])], 8).is_err());
        assert!(check_row(&vec![PackValue::Null; 9], 8).is_err());
    }

    #[test]
    fn a_refused_row_fails_the_take_and_leaves_the_cursor_in_place() {
        let mut rs = ResultSet::new(vec!["s".into()]);
        rs.push_row(vec![Some(Value::Str("ok".into()))]);
        rs.push_row(vec![Some(Value::Str("x".repeat(20)))]);
        let mut cursor = RowCursor::new(rs, false, None).with_max_len(10);
        let err = cursor.take(None, (5, 0)).unwrap_err();
        assert!(matches!(err, BoltError::TooLarge(_)));
        assert!(err.message().contains("PackStream"));
        assert_eq!(cursor.take(Some(1), (5, 0)).unwrap().len(), 1);
    }

    #[test]
    fn a_row_past_the_size_limit_fails_the_pull_and_sends_nothing() {
        use crate::connection::Connection;
        use crate::messages::hello::{AuthToken, Hello, TokenAuthenticator};
        use crate::messages::{ClientMessage, MessageKind, ServerMessage};
        use std::sync::Arc;

        let mut conn = Connection::new(
            Arc::new(TokenAuthenticator::new(None, Default::default())),
            Duration::from_secs(30),
            "c".into(),
            (5, 0),
        );
        conn.handle(ClientMessage::Hello(Hello {
            user_agent: None,
            auth: AuthToken::None,
        }));
        // The real limit is 4 GiB; a small one stands in so the test allocates
        // nothing large.
        let mut rows = ResultSet::new(vec!["s".into()]);
        rows.push_row(vec![Some(Value::Str("fine".into()))]);
        rows.push_row(vec![Some(Value::Str("x".repeat(64)))]);
        let cursor = RowCursor::new(rows, false, None).with_max_len(32);
        assert!(conn.enter_streaming(Box::new(cursor)).is_ok());
        let pull = ClientMessage::Other(
            MessageKind::Pull,
            vec![PackValue::Map(vec![("n".into(), PackValue::Int(-1))])],
        );
        let out = conn.handle(pull);
        let [ServerMessage::Failure(err)] = out.as_slice() else {
            panic!("exactly one FAILURE, no RECORD before it: {out:?}")
        };
        assert!(matches!(err, BoltError::TooLarge(_)));
        assert!(err.message().contains("PackStream size limit"));
        assert_eq!(conn.state().name(), "Failed");
    }

    #[test]
    fn a_too_large_row_from_a_committed_write_says_the_write_committed() {
        let mut rows = ResultSet::new(vec!["s".into()]);
        rows.push_row(vec![Some(Value::Str("x".repeat(64)))]);
        let mut write =
            RowCursor::new(rows.clone(), true, Some("mushroomdb:commit:1".into())).with_max_len(8);
        let err = write.take(None, (5, 0)).unwrap_err();
        assert!(err.message().contains("already committed"), "{err}");
        let mut read = RowCursor::new(rows.clone(), false, Some("b".into())).with_max_len(8);
        assert!(!read
            .take(None, (5, 0))
            .unwrap_err()
            .message()
            .contains("committed"));
        // A write inside a transaction has not committed.
        let mut buffered = RowCursor::new(rows, true, None).with_max_len(8);
        assert!(!buffered
            .take(None, (5, 0))
            .unwrap_err()
            .message()
            .contains("committed"));
    }

    #[test]
    fn a_bolt_null_parameter_is_value_null_at_every_depth() {
        let req = RunRequest::from_fields(vec![
            PackValue::String("q".into()),
            PackValue::Map(vec![
                ("kept".into(), PackValue::Int(1)),
                ("top".into(), PackValue::Null),
                (
                    "map".into(),
                    PackValue::Map(vec![
                        ("a".into(), PackValue::Int(1)),
                        ("b".into(), PackValue::Null),
                    ]),
                ),
                (
                    "list".into(),
                    PackValue::List(vec![PackValue::Int(1), PackValue::Null]),
                ),
            ]),
            PackValue::Map(vec![]),
        ])
        .unwrap();
        let params = req.params((5, 0)).unwrap();
        assert_eq!(params.get("kept"), Some(&Value::Int(1)));
        assert_eq!(params.get("top"), Some(&Value::Null));
        assert_eq!(
            params.get("list"),
            Some(&Value::List(vec![Value::Int(1), Value::Null]))
        );
        assert_eq!(
            params.get("map"),
            Some(&Value::Map(
                [
                    ("a".to_string(), Value::Int(1)),
                    ("b".to_string(), Value::Null)
                ]
                .into()
            ))
        );
    }
}
