//! Interactive transactions over HTTP: `POST /tx/begin`, `POST /tx/{id}/run`,
//! `POST /tx/{id}/commit`, `POST /tx/{id}/rollback`, `DELETE /tx/{id}` and
//! `GET /tx/{id}`.
//!
//! # Where the state lives
//!
//! An open transaction is a [`Transaction`] (an owned, `Send` handle that holds
//! no lock) kept in a [`TxStore`], a map from a random 128-bit id to the
//! transaction, the identity that began it and an idle deadline. The state is in
//! memory: a restart loses every open transaction, which is a rollback.
//!
//! # Rules the store enforces
//!
//! - **Ownership.** A transaction belongs to the credential that began it (the
//!   bearer token itself is the owner key; a server with no authentication has
//!   one shared owner, so every caller can reach every transaction). Any
//!   other caller gets 404 for it, the answer for an id that never existed, so
//!   an id reveals nothing to someone who is not its owner.
//! - **Idle timeout.** `MUSHROOMDB_TX_TIMEOUT_SECS` (default 30) from the last
//!   request that finished on the transaction. A background thread rolls back
//!   what expired; a request that arrives first is answered 410 and rolls it
//!   back itself. An expired id answers 410 to its owner for a while, then 404.
//! - **Limit.** At most `MUSHROOMDB_TX_MAX` (default 100) open at once; a
//!   `begin` beyond it is 503 with `Retry-After`.
//! - **Lifetime.** `MUSHROOMDB_TX_MAX_LIFETIME_SECS` (default 300) from `begin`,
//!   however often it is used: a client that runs a statement every few
//!   seconds cannot pin its snapshot and write buffer forever. Past it the
//!   transaction is rolled back, even if a request is running on it.
//! - **Memory.** An open transaction holds its snapshot and every write it
//!   buffered, up to a request body (64 MiB) per statement. The cap on open
//!   transactions and the lifetime bound the total; they do not make one
//!   transaction small.
//! - **One request at a time.** A request on a transaction checks it out of the
//!   map and puts it back when it ends. A second request in between is 409
//!   "transaction busy": two statements never interleave on one transaction.
//!   The hand-back runs on the blocking thread that ran the statement, not in
//!   the request's future, so a request that is cancelled (client gone, 408,
//!   load shed) cannot strand the transaction; a drop guard ends it if the
//!   statement panics.
//! - **Shutdown.** [`TxStore::close`] rolls back every open transaction and
//!   makes `begin` answer 503. The server calls it when the shutdown signal
//!   arrives, which is the start of the drain phase. A transaction that is
//!   mid-request then is rolled back when that request ends.
//!
//! # Identity and scope
//!
//! A full token, an `admin` key and a `write` key run the transaction engine
//! itself: reads see the transaction's own writes and a commit is one WAL
//! frame. A role token and a `read` key are scoped: the role's mask is
//! resolved at `begin` and every read runs through it on the snapshot the
//! transaction began on. Such a transaction never writes (403, the refusal
//! `/query` gives), because the transaction engine has no role-scoped write
//! path; its commit writes nothing and returns the snapshot's commit number.

use super::{
    err_response, forbidden, graph_err, json_ok, request_token, role_mask_err, AppState,
    AuthIdentity, ReadOnlyKey, READ_TIER_REFUSAL,
};
use crate::json::{params_from_json, result_set_json};
use axum::extract::{Extension, Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mushroomdb::{
    is_write_query, GraphError, NodeMask, ReaderSnapshot, ResultSet, Transaction, Value,
};
use serde_json::{json, Value as Js};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Environment variable: seconds a transaction may sit idle.
pub const TX_TIMEOUT_ENV: &str = "MUSHROOMDB_TX_TIMEOUT_SECS";
/// Environment variable: seconds a transaction may live, used or not.
pub const TX_MAX_LIFETIME_ENV: &str = "MUSHROOMDB_TX_MAX_LIFETIME_SECS";
/// Environment variable: most transactions open at once.
pub const TX_MAX_ENV: &str = "MUSHROOMDB_TX_MAX";

const DEFAULT_IDLE_SECS: u64 = 30;
const DEFAULT_MAX_OPEN: usize = 100;
const DEFAULT_LIFETIME_SECS: u64 = 300;
/// How many expired ids are remembered, so their owner hears 410 and not 404.
const MAX_TOMBSTONES: usize = 10_000;
/// The text of a bookmark; the Bolt `COMMIT` reply carries the same one.
const BOOKMARK_PREFIX: &str = "mushroomdb:commit:";

/// A commit number as the text a client hands to a later request.
fn bookmark_text(seq: u64) -> String {
    format!("{BOOKMARK_PREFIX}{seq}")
}

/// The limits on open transactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxLimits {
    /// Most transactions open at once; a `begin` beyond it is 503.
    pub max_open: usize,
    /// How long a transaction may sit idle before it is rolled back.
    pub idle_timeout: Duration,
    /// How long a transaction may live from `begin`, however busy.
    pub max_lifetime: Duration,
}

impl Default for TxLimits {
    fn default() -> Self {
        Self {
            max_open: DEFAULT_MAX_OPEN,
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_SECS),
            max_lifetime: Duration::from_secs(DEFAULT_LIFETIME_SECS),
        }
    }
}

impl TxLimits {
    /// The limits `MUSHROOMDB_TX_MAX`, `MUSHROOMDB_TX_TIMEOUT_SECS` and
    /// `MUSHROOMDB_TX_MAX_LIFETIME_SECS` name. A
    /// value that is unset keeps the default; one that is not a positive
    /// integer keeps the default too and logs a warning.
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`TxLimits::from_env`] reading the three variables through `lookup`
    /// instead of the process environment.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        Self::from_values(
            lookup(TX_MAX_ENV).as_deref(),
            lookup(TX_TIMEOUT_ENV).as_deref(),
            lookup(TX_MAX_LIFETIME_ENV).as_deref(),
        )
    }

    fn from_values(
        max_open: Option<&str>,
        idle_secs: Option<&str>,
        lifetime_secs: Option<&str>,
    ) -> Self {
        let positive = |name: &str, raw: Option<&str>| -> Option<u64> {
            let raw = raw?;
            match raw.trim().parse::<u64>() {
                Ok(n) if n > 0 => Some(n),
                _ => {
                    tracing::warn!("{name}={raw:?} is not a positive integer; using the default");
                    None
                }
            }
        };
        let defaults = Self::default();
        Self {
            max_open: positive(TX_MAX_ENV, max_open).map_or(defaults.max_open, |n| {
                usize::try_from(n).unwrap_or(usize::MAX)
            }),
            idle_timeout: positive(TX_TIMEOUT_ENV, idle_secs)
                .map_or(defaults.idle_timeout, Duration::from_secs),
            max_lifetime: positive(TX_MAX_LIFETIME_ENV, lifetime_secs)
                .map_or(defaults.max_lifetime, Duration::from_secs),
        }
    }
}

/// What an open transaction runs on.
pub(crate) enum TxBody {
    /// The transaction engine: writes buffer and commit as one WAL frame.
    Full(Box<Transaction>),
    /// A role's read-only view of the snapshot the transaction began on.
    Scoped {
        snapshot: ReaderSnapshot,
        mask: NodeMask,
        /// A `read`-tier key, whose refusal text differs from a role token's.
        read_tier_key: bool,
        /// The clock at BEGIN: what `.transaction()` reads in each statement.
        clock_micros: i64,
    },
}

struct Slot {
    owner: String,
    /// When `begin` ran; the absolute lifetime counts from here.
    born: Instant,
    deadline: Instant,
    deadline_wall: SystemTime,
    op_count: usize,
    /// `None` while a request has the transaction checked out.
    body: Option<TxBody>,
}

struct Tombstone {
    owner: String,
    expired_at: Instant,
}

#[derive(Default)]
struct Registry {
    open: HashMap<String, Slot>,
    expired: HashMap<String, Tombstone>,
    closed: bool,
}

struct Inner {
    limits: TxLimits,
    state: Mutex<Registry>,
    reaper: Once,
}

/// Why a store call failed; [`TxError::into_response`] gives the HTTP answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TxError {
    /// No such transaction, or not the caller's.
    NotFound,
    /// The idle timeout rolled it back.
    Expired,
    /// Another request has it checked out.
    Busy,
    /// The server is shutting down.
    Closed,
    /// The open-transaction limit is reached.
    TooMany(usize),
}

impl IntoResponse for TxError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "unknown transaction".to_string()),
            Self::Expired => (
                StatusCode::GONE,
                "transaction expired: idle too long, rolled back".to_string(),
            ),
            Self::Busy => (StatusCode::CONFLICT, "transaction busy".to_string()),
            Self::Closed => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server is shutting down; no transactions".to_string(),
            ),
            Self::TooMany(limit) => (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("too many open transactions (limit {limit}); retry"),
            ),
        };
        let mut body = json!({ "error": message });
        if self == Self::Busy {
            // Told apart from a commit conflict (`"conflict"`): nothing is
            // wrong with the transaction, the request can be retried.
            body["code"] = json!("busy");
        }
        let mut res = (status, Json(body)).into_response();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        res
    }
}

/// The open transactions of one server. Cheap to clone; clones share state.
#[derive(Clone)]
pub(crate) struct TxStore {
    inner: Arc<Inner>,
}

fn lock(state: &Mutex<Registry>) -> MutexGuard<'_, Registry> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Equality that does not stop at the first differing byte: the owner is a
/// bearer token.
fn same_owner(a: &str, b: &str) -> bool {
    super::constant_time_eq(a.as_bytes(), b.as_bytes())
}

impl TxStore {
    pub(crate) fn new(limits: TxLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                limits,
                state: Mutex::new(Registry::default()),
                reaper: Once::new(),
            }),
        }
    }

    /// Whether a `begin` could be accepted right now. Checked before the
    /// snapshot is frozen, so a flood of begins at the limit does no work.
    pub(crate) fn has_room(&self) -> Result<(), TxError> {
        let mut state = lock(&self.inner.state);
        if state.closed {
            return Err(TxError::Closed);
        }
        let reaped = self.reap_locked(&mut state, Instant::now());
        let full = state.open.len() >= self.inner.limits.max_open;
        drop(state);
        drop(reaped);
        if full {
            return Err(TxError::TooMany(self.inner.limits.max_open));
        }
        Ok(())
    }

    /// Register a new transaction for `owner`; returns its id and when it
    /// expires.
    pub(crate) fn begin(
        &self,
        owner: String,
        body: TxBody,
    ) -> Result<(String, SystemTime), TxError> {
        self.start_reaper();
        let now = Instant::now();
        let id = format!("{:032x}", rand::random::<u128>());
        let (deadline, deadline_wall) = self.next_deadline(now, now);
        let reaped = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return Err(TxError::Closed);
            }
            // Count only live ones, whether or not the reaper has run yet.
            let reaped = self.reap_locked(&mut state, now);
            if state.open.len() >= self.inner.limits.max_open {
                drop(state);
                drop(reaped);
                return Err(TxError::TooMany(self.inner.limits.max_open));
            }
            state.open.insert(
                id.clone(),
                Slot {
                    owner,
                    born: now,
                    deadline,
                    deadline_wall,
                    op_count: 0,
                    body: Some(body),
                },
            );
            reaped
        };
        drop(reaped);
        Ok((id, deadline_wall))
    }

    /// Take the transaction out of the map for one request. While it is out, a
    /// second request on it is [`TxError::Busy`].
    pub(crate) fn checkout(&self, id: &str, owner: &str) -> Result<TxBody, TxError> {
        let now = Instant::now();
        let mut state = lock(&self.inner.state);
        let Some(slot) = state.open.get_mut(id) else {
            return Err(Self::missing(&state, id, owner));
        };
        if !same_owner(&slot.owner, owner) {
            return Err(TxError::NotFound);
        }
        let Some(body) = slot.body.take() else {
            return Err(TxError::Busy);
        };
        if slot.deadline <= now {
            // Expired before the reaper got to it: it is rolled back now.
            slot.body = Some(body);
            let reaped = self.expire(&mut state, id, now);
            drop(state);
            drop(reaped);
            return Err(TxError::Expired);
        }
        Ok(body)
    }

    /// Return a checked-out transaction and restart its idle timer. `ran` counts
    /// the request in `op_count`. If the server began shutting down meanwhile
    /// the transaction is rolled back and the request is refused.
    pub(crate) fn checkin(&self, id: &str, body: TxBody, ran: bool) -> Result<(), TxError> {
        let now = Instant::now();
        let mut state = lock(&self.inner.state);
        if state.closed {
            let slot = state.open.remove(id);
            drop(state);
            drop((slot, body));
            return Err(TxError::Closed);
        }
        let was_expired = state.expired.contains_key(id);
        match state.open.get_mut(id) {
            Some(slot) => {
                let (deadline, deadline_wall) = self.next_deadline(now, slot.born);
                slot.body = Some(body);
                slot.deadline = deadline;
                slot.deadline_wall = deadline_wall;
                slot.op_count += usize::from(ran);
                Ok(())
            }
            // Removed while the statement ran: past its lifetime.
            None if was_expired => Err(TxError::Expired),
            None => Err(TxError::NotFound),
        }
    }

    /// Forget a transaction that ended (committed, rolled back, conflicted).
    pub(crate) fn finish(&self, id: &str) {
        let slot = lock(&self.inner.state).open.remove(id);
        drop(slot);
    }

    /// What `GET /tx/{id}` reports. Does not restart the idle timer.
    pub(crate) fn info(&self, id: &str, owner: &str) -> Result<(SystemTime, usize), TxError> {
        let now = Instant::now();
        let mut state = lock(&self.inner.state);
        let Some(slot) = state.open.get(id) else {
            return Err(Self::missing(&state, id, owner));
        };
        if !same_owner(&slot.owner, owner) {
            return Err(TxError::NotFound);
        }
        if slot.body.is_some() && slot.deadline <= now {
            let reaped = self.expire(&mut state, id, now);
            drop(state);
            drop(reaped);
            return Err(TxError::Expired);
        }
        Ok((slot.deadline_wall, slot.op_count))
    }

    /// Roll back every open transaction and refuse new ones. A transaction a
    /// request has checked out is rolled back when that request ends.
    pub(crate) fn close(&self) -> usize {
        let dropped: Vec<Slot> = {
            let mut state = lock(&self.inner.state);
            state.closed = true;
            let idle: Vec<String> = state
                .open
                .iter()
                .filter(|(_, slot)| slot.body.is_some())
                .map(|(id, _)| id.clone())
                .collect();
            idle.iter().filter_map(|id| state.open.remove(id)).collect()
        };
        dropped.len()
    }

    /// Roll back every idle transaction whose deadline is at or before `now`.
    /// Returns how many.
    pub(crate) fn reap(&self, now: Instant) -> usize {
        let reaped = {
            let mut state = lock(&self.inner.state);
            self.reap_locked(&mut state, now)
        };
        reaped.len()
    }

    /// How many transactions are open.
    pub(crate) fn open_count(&self) -> usize {
        lock(&self.inner.state).open.len()
    }

    /// The idle deadline from `now`, never past `born` plus the lifetime.
    fn next_deadline(&self, now: Instant, born: Instant) -> (Instant, SystemTime) {
        let limits = &self.inner.limits;
        let deadline = (now + limits.idle_timeout).min(born + limits.max_lifetime);
        (
            deadline,
            SystemTime::now() + deadline.saturating_duration_since(now),
        )
    }

    /// The answer for an id that is not open: 410 to the owner of an id that
    /// expired, 404 to anyone else.
    fn missing(state: &Registry, id: &str, owner: &str) -> TxError {
        match state.expired.get(id) {
            Some(t) if same_owner(&t.owner, owner) => TxError::Expired,
            _ => TxError::NotFound,
        }
    }

    /// Remove `id` as expired. The removed slot is returned so the caller drops
    /// it (the rollback) after releasing the lock.
    fn expire(&self, state: &mut Registry, id: &str, now: Instant) -> Option<Slot> {
        let slot = state.open.remove(id)?;
        self.remember(state, id, &slot.owner, now);
        Some(slot)
    }

    fn remember(&self, state: &mut Registry, id: &str, owner: &str, now: Instant) {
        if state.expired.len() >= MAX_TOMBSTONES {
            let oldest = state
                .expired
                .iter()
                .min_by_key(|(_, t)| t.expired_at)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                state.expired.remove(&oldest);
            }
        }
        state.expired.insert(
            id.to_string(),
            Tombstone {
                owner: owner.to_string(),
                expired_at: now,
            },
        );
    }

    fn reap_locked(&self, state: &mut Registry, now: Instant) -> Vec<Slot> {
        let retention = (self.inner.limits.idle_timeout * 10).max(Duration::from_secs(60));
        state
            .expired
            .retain(|_, t| now.saturating_duration_since(t.expired_at) < retention);
        let due: Vec<String> = state
            .open
            .iter()
            .filter(|(_, slot)| {
                // Idle past its deadline, or past its lifetime even while a
                // request holds it (which also frees a slot whose request was
                // lost some other way).
                (slot.body.is_some() && slot.deadline <= now)
                    || slot.born + self.inner.limits.max_lifetime <= now
            })
            .map(|(id, _)| id.clone())
            .collect();
        due.iter()
            .filter_map(|id| self.expire(state, id, now))
            .collect()
    }

    /// Start the background thread that rolls back expired transactions, once.
    /// It holds only a weak reference, so it ends when the server is dropped.
    fn start_reaper(&self) {
        self.inner.reaper.call_once(|| {
            let weak: Weak<Inner> = Arc::downgrade(&self.inner);
            let tick = (self.inner.limits.idle_timeout / 4)
                .clamp(Duration::from_millis(10), Duration::from_secs(1));
            let spawned =
                std::thread::Builder::new()
                    .name("tx-reaper".into())
                    .spawn(move || loop {
                        std::thread::sleep(tick);
                        let Some(inner) = weak.upgrade() else { break };
                        let store = TxStore { inner };
                        if lock(&store.inner.state).closed {
                            break;
                        }
                        let n = store.reap(Instant::now());
                        if n > 0 {
                            tracing::debug!(n, "rolled back idle transactions");
                        }
                    });
            if let Err(e) = spawned {
                // Expiry is still enforced on access and at `begin`.
                tracing::error!("could not start the transaction reaper: {e}");
            }
        });
    }
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

/// The credential a request presented: who owns the transactions it begins.
#[derive(Clone, Debug)]
struct Principal(String);

/// Runs inside the auth middleware, so the credential here already resolved.
async fn principal(mut req: Request, next: Next) -> Response {
    let who = Principal(request_token(&req).unwrap_or_default());
    req.extensions_mut().insert(who);
    next.run(req).await
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/tx/begin", post(begin))
        .route("/tx/{id}", get(info))
        .route("/tx/{id}", axum::routing::delete(rollback))
        .route("/tx/{id}/run", post(run))
        .route("/tx/{id}/commit", post(commit))
        .route("/tx/{id}/rollback", post(rollback))
        .route_layer(middleware::from_fn(principal))
}

/// `2026-10-22T12:34:56.789Z`.
fn iso8601(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = i64::try_from(since.as_secs()).unwrap_or(i64::MAX / 2);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Hinnant's `civil_from_days`).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60,
        since.subsec_millis()
    )
}

async fn begin(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Extension(who): Extension<Principal>,
    read_only_key: Option<Extension<ReadOnlyKey>>,
) -> Response {
    // Before the snapshot is frozen: a begin that will be refused does no work.
    if let Err(e) = state.txs.has_room() {
        return e.into_response();
    }
    let db = state.db.clone();
    let body = match identity {
        AuthIdentity::Role(role) => {
            let snapshot = db.reader();
            let mask = match snapshot.mask_for_role(&role) {
                Ok(mask) => mask,
                Err(e) => return role_mask_err(e),
            };
            TxBody::Scoped {
                snapshot,
                mask,
                read_tier_key: read_only_key.is_some(),
                clock_micros: mushroomdb::clock_micros(),
            }
        }
        // `Anonymous` reaches only `/health`; refuse it here all the same.
        AuthIdentity::Anonymous => return forbidden("a credential is required"),
        AuthIdentity::Full | AuthIdentity::Write => {
            // Freezing the snapshot reads under the store's read lock.
            match tokio::task::spawn_blocking(move || db.begin_transaction()).await {
                Ok(tx) => TxBody::Full(Box::new(tx)),
                Err(_) => return err_response("begin task panicked"),
            }
        }
    };
    match state.txs.begin(who.0, body) {
        Ok((id, expires)) => json_ok(json!({"tx_id": id, "expires_at": iso8601(expires)})),
        Err(e) => e.into_response(),
    }
}

async fn info(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    match state.txs.info(&id, &who.0) {
        Ok((expires, op_count)) => json_ok(json!({
            "tx_id": id,
            "expires_at": iso8601(expires),
            "op_count": op_count,
        })),
        Err(e) => e.into_response(),
    }
}

/// Why a statement did not run.
enum RunError {
    Engine(GraphError),
    /// 400: the text is not a statement this path can classify or run.
    Bad(String),
    /// 403, with the refusal text.
    Refused(&'static str),
}

impl RunError {
    fn into_response(self) -> Response {
        match self {
            Self::Engine(e) => graph_err(e),
            Self::Bad(detail) => err_response(detail),
            Self::Refused(detail) => forbidden(detail),
        }
    }
}

/// One statement on a transaction body, on a blocking thread.
fn execute(
    body: &mut TxBody,
    cypher: &str,
    params: &BTreeMap<String, Value>,
) -> Result<ResultSet, RunError> {
    match body {
        TxBody::Full(tx) => tx.execute(cypher, params).map_err(RunError::Engine),
        TxBody::Scoped {
            snapshot,
            mask,
            read_tier_key,
            clock_micros,
        } => {
            if is_write_query(cypher).map_err(RunError::Bad)? {
                return Err(RunError::Refused(if *read_tier_key {
                    READ_TIER_REFUSAL
                } else {
                    "role-bound token: writes are not permitted inside a transaction"
                }));
            }
            snapshot
                .query_masked_in_transaction(cypher, params, mask, *clock_micros)
                .map_err(|e| match e {
                    GraphError::MaskedReadOnly => {
                        RunError::Bad("masked queries are read-only".into())
                    }
                    other => RunError::Engine(other),
                })
        }
    }
}

/// Ends a transaction's slot when dropped unless disarmed. The hand-back
/// runs on a blocking thread; this covers that thread panicking.
struct Lease<'a> {
    store: &'a TxStore,
    id: &'a str,
    armed: bool,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.store.finish(self.id);
        }
    }
}

async fn run(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
    Json(request): Json<Js>,
) -> Response {
    let Some(cypher) = request.get("cypher").and_then(Js::as_str).map(String::from) else {
        return err_response("missing cypher");
    };
    let params = match params_from_json(request.get("params")) {
        Ok(p) => p,
        Err(e) => return err_response(e),
    };
    let mut body = match state.txs.checkout(&id, &who.0) {
        Ok(body) => body,
        Err(e) => return e.into_response(),
    };
    // Nothing awaits between the checkout and this spawn. The statement and
    // the hand-back both run inside the blocking task, which finishes even if
    // this future is dropped (client gone, 408, load shed), so the slot
    // cannot be stranded checked out.
    let store = state.txs.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let mut lease = Lease {
            store: &store,
            id: &id,
            armed: true,
        };
        let out = execute(&mut body, &cypher, &params);
        let handed_back = store.checkin(&id, body, out.is_ok());
        lease.armed = false;
        (out, handed_back)
    })
    .await;
    let Ok((out, handed_back)) = joined else {
        return err_response("statement task panicked; transaction rolled back");
    };
    if let Err(e) = handed_back {
        return e.into_response();
    }
    match out {
        Ok(rs) => json_ok(result_set_json(&rs)),
        Err(e) => e.into_response(),
    }
}

async fn commit(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let body = match state.txs.checkout(&id, &who.0) {
        Ok(body) => body,
        Err(e) => return e.into_response(),
    };
    let store = state.txs.clone();
    let joined = tokio::task::spawn_blocking(move || {
        // The transaction is consumed whatever the outcome, even if the
        // request that asked is gone.
        let _lease = Lease {
            store: &store,
            id: &id,
            armed: true,
        };
        match body {
            TxBody::Full(tx) => tx.commit(),
            // A scoped transaction wrote nothing: its bookmark is the commit it read.
            TxBody::Scoped { snapshot, .. } => Ok(snapshot.commit_seq()),
        }
    })
    .await;
    match joined {
        Ok(Ok(seq)) => json_ok(json!({
            "bookmark": seq,
            "bookmark_text": bookmark_text(seq),
        })),
        Ok(Err(conflict @ GraphError::CasConflict { .. })) => (
            StatusCode::CONFLICT,
            Json(json!({"error": conflict.to_string(), "code": "conflict"})),
        )
            .into_response(),
        Ok(Err(e)) => graph_err(e),
        Err(_) => err_response("commit task panicked"),
    }
}

/// `POST /tx/{id}/rollback` and `DELETE /tx/{id}`.
async fn rollback(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    match state.txs.checkout(&id, &who.0) {
        Ok(body) => {
            state.txs.finish(&id);
            drop(body);
            json_ok(json!({"rolled_back": true}))
        }
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mushroomdb::SharedDb;

    fn store(max_open: usize, idle_ms: u64) -> TxStore {
        TxStore::new(TxLimits {
            max_open,
            idle_timeout: Duration::from_millis(idle_ms),
            max_lifetime: Duration::from_secs(300),
        })
    }

    fn body() -> (TxBody, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        (TxBody::Full(Box::new(db.begin_transaction())), dir)
    }

    /// QL-CL (row 161): a role-scoped transaction holds the instant it began
    /// at for `.transaction()`, in every statement.
    #[test]
    fn a_scoped_transaction_reads_one_transaction_instant() {
        const BEGAN: i64 = 1_700_000_000_000_000;
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        let mut body = TxBody::Scoped {
            snapshot: db.reader(),
            mask: mushroomdb::NodeMask::from_ids([]),
            read_tier_key: false,
            clock_micros: BEGAN,
        };
        let q = "RETURN datetime.transaction() = datetime.fromepoch(1700000000, 0) AS began, \
                 datetime.statement() = datetime.fromepoch(1700000000, 0) AS stmt";
        for _ in 0..2 {
            let Ok(rs) = execute(&mut body, q, &BTreeMap::new()) else {
                panic!("the statement failed")
            };
            assert_eq!(rs.get(0, "began"), Some(&mushroomdb::Value::Bool(true)));
            // The statement's own instant is the clock's, not BEGIN's.
            assert_eq!(rs.get(0, "stmt"), Some(&mushroomdb::Value::Bool(false)));
        }
    }

    #[test]
    fn limits_default_and_parse() {
        assert_eq!(TxLimits::from_values(None, None, None), TxLimits::default());
        let l = TxLimits::from_values(Some("7"), Some(" 5 "), Some("9"));
        assert_eq!(
            (l.max_open, l.idle_timeout, l.max_lifetime),
            (7, Duration::from_secs(5), Duration::from_secs(9))
        );
        // Zero, text and negative numbers keep the default.
        for bad in ["0", "x", "-1", ""] {
            assert_eq!(
                TxLimits::from_values(Some(bad), Some(bad), Some(bad)),
                TxLimits::default(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn iso8601_formats_known_instants() {
        let at = |secs: u64, millis: u32| UNIX_EPOCH + Duration::new(secs, millis * 1_000_000);
        assert_eq!(iso8601(at(0, 0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(at(951_782_400, 5)), "2000-02-29T00:00:00.005Z");
        assert_eq!(iso8601(at(1_700_000_000, 999)), "2023-11-14T22:13:20.999Z");
    }

    #[test]
    fn a_second_checkout_is_busy_until_checkin() {
        let s = store(10, 60_000);
        let (b, _dir) = body();
        let (id, _) = s.begin("alice".into(), b).unwrap();
        let held = s.checkout(&id, "alice").unwrap();
        assert_eq!(s.checkout(&id, "alice").err(), Some(TxError::Busy));
        // Busy is not a reason to tell a stranger anything else.
        assert_eq!(s.checkout(&id, "mallory").err(), Some(TxError::NotFound));
        s.checkin(&id, held, true).unwrap();
        assert!(s.checkout(&id, "alice").is_ok());
    }

    #[test]
    fn begin_refuses_beyond_the_limit_and_counts_only_live() {
        let s = store(1, 60_000);
        let (b1, _d1) = body();
        let (id, _) = s.begin("a".into(), b1).unwrap();
        let (b2, _d2) = body();
        assert_eq!(s.begin("a".into(), b2).err(), Some(TxError::TooMany(1)));
        s.finish(&id);
        let (b3, _d3) = body();
        assert!(s.begin("a".into(), b3).is_ok());
    }

    #[test]
    fn reap_rolls_back_idle_expired_and_remembers_the_owner() {
        let s = store(10, 100);
        let (b, _dir) = body();
        let (id, _) = s.begin("alice".into(), b).unwrap();
        assert_eq!(s.reap(Instant::now()), 0);
        assert_eq!(s.reap(Instant::now() + Duration::from_secs(1)), 1);
        assert_eq!(s.open_count(), 0);
        assert_eq!(s.checkout(&id, "alice").err(), Some(TxError::Expired));
        assert_eq!(s.checkout(&id, "mallory").err(), Some(TxError::NotFound));
    }

    #[test]
    fn a_checked_out_transaction_is_not_reaped() {
        let s = store(10, 100);
        let (b, _dir) = body();
        let (id, _) = s.begin("alice".into(), b).unwrap();
        let held = s.checkout(&id, "alice").unwrap();
        assert_eq!(s.reap(Instant::now() + Duration::from_secs(1)), 0);
        s.checkin(&id, held, true).unwrap();
        assert_eq!(s.info(&id, "alice").unwrap().1, 1);
    }

    #[test]
    fn close_rolls_back_and_refuses_begin() {
        let s = store(10, 60_000);
        let (b1, _d1) = body();
        let (idle, _) = s.begin("a".into(), b1).unwrap();
        let (b2, _d2) = body();
        let (busy, _) = s.begin("a".into(), b2).unwrap();
        let held = s.checkout(&busy, "a").unwrap();
        assert_eq!(s.close(), 1);
        assert_eq!(s.checkout(&idle, "a").err(), Some(TxError::NotFound));
        let (b3, _d3) = body();
        assert_eq!(s.begin("a".into(), b3).err(), Some(TxError::Closed));
        // The one a request held is rolled back when that request ends.
        assert_eq!(s.checkin(&busy, held, true).err(), Some(TxError::Closed));
        assert_eq!(s.open_count(), 0);
    }

    #[test]
    fn the_reaper_thread_expires_idle_transactions() {
        let s = store(10, 50);
        let (b, _dir) = body();
        let (_id, _) = s.begin("a".into(), b).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.open_count() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(s.open_count(), 0);
    }

    #[test]
    fn the_lifetime_ends_a_transaction_that_keeps_being_used() {
        let s = TxStore::new(TxLimits {
            max_open: 10,
            idle_timeout: Duration::from_secs(60),
            max_lifetime: Duration::from_secs(100),
        });
        let (b, _dir) = body();
        let (id, expires) = s.begin("a".into(), b).unwrap();
        // The reported expiry is the nearer of the two bounds.
        assert!(expires <= SystemTime::now() + Duration::from_secs(61));
        let held = s.checkout(&id, "a").unwrap();
        s.checkin(&id, held, true).unwrap();
        // Past the lifetime it goes, even though it was just used.
        assert_eq!(s.reap(Instant::now() + Duration::from_secs(101)), 1);
        assert_eq!(s.checkout(&id, "a").err(), Some(TxError::Expired));
    }

    #[test]
    fn a_slot_whose_request_was_lost_is_freed_at_the_lifetime() {
        let s = TxStore::new(TxLimits {
            max_open: 1,
            idle_timeout: Duration::from_secs(60),
            max_lifetime: Duration::from_secs(100),
        });
        let (b, _dir) = body();
        let (id, _) = s.begin("a".into(), b).unwrap();
        // Checked out and never returned.
        drop(s.checkout(&id, "a").unwrap());
        assert_eq!(s.checkout(&id, "a").err(), Some(TxError::Busy));
        assert_eq!(s.reap(Instant::now() + Duration::from_secs(101)), 1);
        let (b2, _d2) = body();
        assert!(s.begin("a".into(), b2).is_ok());
    }

    #[test]
    fn has_room_follows_the_limit_and_close() {
        let s = store(1, 60_000);
        assert_eq!(s.has_room(), Ok(()));
        let (b, _dir) = body();
        let (id, _) = s.begin("a".into(), b).unwrap();
        assert_eq!(s.has_room(), Err(TxError::TooMany(1)));
        s.finish(&id);
        assert_eq!(s.has_room(), Ok(()));
        s.close();
        assert_eq!(s.has_room(), Err(TxError::Closed));
    }

    #[test]
    fn bookmark_text_is_the_form_a_bolt_commit_returns() {
        // The same literal the Bolt tests pin.
        assert_eq!(bookmark_text(7), "mushroomdb:commit:7");
    }
}
