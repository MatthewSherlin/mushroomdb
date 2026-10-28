//! The per-connection actor: a state machine over decoded messages, and the
//! read loop that feeds it.
//!
//! [`Connection`] holds no I/O. It takes one [`ClientMessage`] and returns the
//! responses to send, so every transition is testable without a socket.
//! [`serve_connection`] is the loop around it: handshake, deframe, dispatch,
//! write, plus the one-second tick that expires an idle transaction. Given a
//! stop signal ([`ConnectionConfig::shutdown`]) it closes at its next idle
//! point, which rolls back an open transaction.
//!
//! States follow the Bolt 4.4/5.0 server state machine:
//! `NotAuthenticated` (the spec's CONNECTED), `Ready`, `Streaming`, `InTx`
//! (TX_READY and TX_STREAMING), `Failed`, `Interrupted` and `Defunct`.
//!
//! A request that fails on its merits (a handler error) moves to `Failed`, where
//! everything but `RESET` and `GOODBYE` is `IGNORED`. A protocol violation moves
//! to `Defunct`: the server sends one `FAILURE` and closes. The Bolt server
//! state machine (section "Server state `DEFUNCT`") says a connection is
//! defunct "after an unrecoverable error or protocol violation", and its
//! transition tables list only the messages each state accepts, so an
//! unparseable message, an unknown signature, or a well-formed message the
//! current state does not list is a violation. A `RESET` that arrives
//! while earlier messages are still queued interrupts them: they are `IGNORED`
//! and the `RESET` returns the connection to `Ready`.
//!
//! `RUN`, `PULL` and `DISCARD` (see `messages::run`) and `BEGIN`, `COMMIT` and
//! `ROLLBACK` (see `messages::tx`) are handled here, through the
//! [`QueryService`] a connection is given with [`Connection::with_database`],
//! and `ROUTE` through the [`RoutingConfig`] in its config. A connection with
//! no query service answers `RUN` and `BEGIN` with a not-supported `FAILURE`.
//!
//! # RESET while a request is running
//!
//! With a store, [`serve_connection_with`] runs each batch of messages on a
//! blocking thread and keeps reading the socket meanwhile. A `RESET` that
//! arrives sets the connection's interrupt flag ([`Connection::interrupt_flag`]).
//! It takes effect at the first of these points:
//!
//! - a bookmark wait (in `RUN` or `BEGIN`) ends within 10 ms, and the request
//!   is answered `IGNORED`;
//! - before the next message of the batch, which is `IGNORED`, as is everything
//!   queued behind it, up to the `RESET`;
//! - otherwise after the statement or commit that is running returns. A
//!   statement in the engine cannot be interrupted: it finishes, its reply is
//!   sent, and a `COMMIT` or auto-commit write that was already submitted is
//!   durable and reported as such. The open transaction (if the `RESET` was
//!   not for a request that committed it) is then rolled back.
//!
//! The `RESET` itself is answered `SUCCESS` once everything before it has been
//! answered. A client that disconnects during a batch sets the same flag; the
//! transaction is rolled back and the connection ends.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mushroomdb::{SharedDb, Value};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::BoltError;
use crate::framing;
use crate::handshake;
use crate::messages::hello::{hello_success, Identity, TokenAuthenticator};
use crate::messages::reset::reset_success;
use crate::messages::route::{requested_database, RoutingConfig};
use crate::messages::run::{
    format_bookmark, QueryService, RunRequest, StreamRequest, DEFAULT_BOOKMARK_TIMEOUT,
};
use crate::messages::tx::BeginRequest;
use crate::messages::{ClientMessage, MessageKind, ServerMessage};
use crate::packstream::PackValue;

/// Default idle limit for an open transaction.
pub const DEFAULT_TX_TIMEOUT: Duration = Duration::from_secs(30);

/// Default limit on a single request message.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Default limit on a request message before the client has authenticated.
pub const DEFAULT_PRE_AUTH_MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// An open transaction, as the connection sees it. Dropping it discards its
/// buffer; `rollback` is the explicit form.
pub trait OpenTransaction: Send {
    fn rollback(self: Box<Self>);

    /// Makes the transaction's writes durable as one commit and returns the
    /// commit number (the bookmark). The transaction is consumed whether or
    /// not it succeeds.
    fn commit(self: Box<Self>) -> Result<u64, BoltError>;

    /// Runs one statement in the transaction and returns its rows.
    fn execute(
        &mut self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<Box<dyn ResultCursor>, BoltError>;

    /// Whether the transaction applies a role's mask itself. A role token's
    /// or `read` key's statements run only in one that does; any other is
    /// refused to them.
    fn is_scoped(&self) -> bool {
        false
    }
}

/// An open result cursor. Dropping it closes it.
pub trait ResultCursor: Send {
    /// The column names, as `RUN` reports them.
    fn fields(&self) -> &[String];

    /// Removes up to `limit` rows (all of them for `None`) and returns them as
    /// `RECORD` fields. `version` is the negotiated Bolt version, which picks
    /// some value encodings. Fails, leaving the cursor to be dropped, for a row
    /// the wire format cannot carry.
    fn take(
        &mut self,
        limit: Option<usize>,
        version: (u8, u8),
    ) -> Result<Vec<Vec<PackValue>>, BoltError>;

    /// Drops up to `limit` rows (all of them for `None`).
    fn discard(&mut self, limit: Option<usize>);

    fn has_more(&self) -> bool;

    /// The metadata for the `SUCCESS` that ends the stream.
    fn summary(&self) -> Vec<(String, PackValue)>;
}

/// Where a connection is in the protocol.
pub enum ConnectionState {
    /// Before `HELLO`.
    NotAuthenticated,
    Ready,
    /// An auto-commit result is open. `last_activity` is the last message
    /// received; the idle limit counts from it, as it does for a transaction.
    Streaming {
        cursor: Box<dyn ResultCursor>,
        last_activity: Instant,
    },
    /// Between `BEGIN` and `COMMIT`/`ROLLBACK`. `last_activity` is the last
    /// message received; the idle limit counts from it. `cursors` are the
    /// results still open, by `qid` (they are handed out in increasing order,
    /// so the last key is the latest).
    InTx {
        tx: Box<dyn OpenTransaction>,
        opened_at: Instant,
        last_activity: Instant,
        cursors: BTreeMap<i64, Box<dyn ResultCursor>>,
    },
    /// A request failed; only `RESET` and `GOODBYE` are acted on.
    Failed,
    /// A `RESET` is being applied; queued requests are ignored.
    Interrupted,
    /// The connection must be closed.
    Defunct,
}

impl ConnectionState {
    pub fn name(&self) -> &'static str {
        match self {
            ConnectionState::NotAuthenticated => "NotAuthenticated",
            ConnectionState::Ready => "Ready",
            ConnectionState::Streaming { .. } => "Streaming",
            ConnectionState::InTx { cursors, .. } if cursors.is_empty() => "InTx",
            ConnectionState::InTx { .. } => "InTxStreaming",
            ConnectionState::Failed => "Failed",
            ConnectionState::Interrupted => "Interrupted",
            ConnectionState::Defunct => "Defunct",
        }
    }
}

impl std::fmt::Debug for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The protocol state of one client connection.
pub struct Connection {
    state: ConnectionState,
    identity: Option<Identity>,
    authenticator: Arc<TokenAuthenticator>,
    tx_timeout: Duration,
    connection_id: String,
    version: (u8, u8),
    /// The failure to report for the next request after the connection failed
    /// without a request to answer (a transaction timeout).
    pending_failure: Option<BoltError>,
    /// Answers `ROUTE` and database selection; `ROUTE` is not supported
    /// without it.
    routing: Option<RoutingConfig>,
    service: Option<Arc<QueryService>>,
    /// The `qid` handed out for the last statement run in a transaction.
    last_qid: i64,
    /// Set (from another task) when a `RESET` is waiting behind the request
    /// now running; cleared when the `RESET` is handled.
    interrupted: Arc<AtomicBool>,
}

impl Connection {
    pub fn new(
        authenticator: Arc<TokenAuthenticator>,
        tx_timeout: Duration,
        connection_id: String,
        version: (u8, u8),
    ) -> Self {
        Self {
            state: ConnectionState::NotAuthenticated,
            identity: None,
            authenticator,
            tx_timeout,
            connection_id,
            version,
            pending_failure: None,
            routing: None,
            service: None,
            last_qid: -1,
            interrupted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The flag the read loop sets when a `RESET` arrives while this
    /// connection is busy with an earlier request. See the module docs.
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        self.interrupted.clone()
    }

    /// Sets the routing config that answers `ROUTE`.
    pub fn with_routing(mut self, routing: Option<RoutingConfig>) -> Self {
        self.routing = routing;
        self
    }

    /// Gives the connection a store to run `RUN` against. Which database names
    /// a `RUN` may carry is decided by the connection's routing config (see
    /// [`with_routing`](Self::with_routing)), the one that answers `ROUTE`.
    /// With none, only an absent or empty name selects the store.
    /// `bookmark_timeout` is how long a statement waits for a bookmark.
    pub fn with_database(mut self, db: SharedDb, bookmark_timeout: Duration) -> Self {
        self.service = Some(Arc::new(
            QueryService::new(db)
                .with_bookmark_timeout(bookmark_timeout)
                .with_version(self.version)
                .with_interrupt(self.interrupted.clone()),
        ));
        self
    }

    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    /// Who the client authenticated as; `None` before `HELLO` succeeds.
    pub fn identity(&self) -> Option<&Identity> {
        self.identity.as_ref()
    }

    /// The negotiated `(major, minor)`.
    pub fn version(&self) -> (u8, u8) {
        self.version
    }

    pub fn is_defunct(&self) -> bool {
        matches!(self.state, ConnectionState::Defunct)
    }

    /// Moves `Ready` to `InTx`. For the `BEGIN` handler; refused in any other
    /// state.
    pub fn enter_tx(
        &mut self,
        tx: Box<dyn OpenTransaction>,
        now: Instant,
    ) -> Result<(), Box<dyn OpenTransaction>> {
        if !matches!(self.state, ConnectionState::Ready) {
            return Err(tx);
        }
        self.state = ConnectionState::InTx {
            tx,
            opened_at: now,
            last_activity: now,
            cursors: BTreeMap::new(),
        };
        Ok(())
    }

    /// Moves `Ready` to `Streaming`. For the auto-commit `RUN` handler;
    /// refused in any other state.
    pub fn enter_streaming(
        &mut self,
        cursor: Box<dyn ResultCursor>,
    ) -> Result<(), Box<dyn ResultCursor>> {
        if !matches!(self.state, ConnectionState::Ready) {
            return Err(cursor);
        }
        self.state = ConnectionState::Streaming {
            cursor,
            last_activity: Instant::now(),
        };
        Ok(())
    }

    /// Marks the connection interrupted because a `RESET` is queued behind
    /// other messages: it rolls back an open transaction and drops a cursor
    /// now, and every message until the `RESET` is `IGNORED`.
    pub fn interrupt(&mut self) {
        match self.state {
            ConnectionState::NotAuthenticated
            | ConnectionState::Defunct
            | ConnectionState::Interrupted => {}
            _ => {
                self.discard_work();
                self.state = ConnectionState::Interrupted;
            }
        }
    }

    /// Rolls back a transaction, or closes an auto-commit result, that has been
    /// idle past the limit. Called on every message and by the one-second
    /// ticker. Returns whether it expired one; the client learns of it as the
    /// `FAILURE` to its next request.
    pub fn check_timeout(&mut self, now: Instant) -> bool {
        let idle = |last: &Instant| now.saturating_duration_since(*last) >= self.tx_timeout;
        let failure = match &self.state {
            ConnectionState::InTx { last_activity, .. } if idle(last_activity) => {
                Some(BoltError::TransactionTimedOut)
            }
            ConnectionState::Streaming { last_activity, .. } if idle(last_activity) => {
                Some(BoltError::Invalid(
                    "the result was idle past its timeout and has been closed".into(),
                ))
            }
            _ => None,
        };
        let expired = failure.is_some();
        if expired {
            self.discard_work();
            self.state = ConnectionState::Failed;
            self.pending_failure = failure;
        }
        expired
    }

    pub fn handle(&mut self, msg: ClientMessage) -> Vec<ServerMessage> {
        self.handle_at(msg, Instant::now())
    }

    /// Handles one request at time `now` and returns the responses to send.
    pub fn handle_at(&mut self, msg: ClientMessage, now: Instant) -> Vec<ServerMessage> {
        self.check_timeout(now);
        match self.state {
            ConnectionState::Defunct => Vec::new(),
            ConnectionState::NotAuthenticated => self.handle_connected(msg),
            ConnectionState::Failed | ConnectionState::Interrupted => self.handle_failed(msg),
            _ => self.handle_active(msg, now),
        }
    }

    /// A request that could not be decoded: a protocol violation, in any state.
    pub fn handle_malformed(&mut self, err: BoltError) -> Vec<ServerMessage> {
        if self.is_defunct() {
            return Vec::new();
        }
        self.violate(err)
    }

    /// Protocol violation: roll back, send one `FAILURE`, become defunct.
    fn violate(&mut self, err: BoltError) -> Vec<ServerMessage> {
        self.discard_work();
        self.state = ConnectionState::Defunct;
        vec![ServerMessage::Failure(err)]
    }

    fn discard_work(&mut self) {
        let state = std::mem::replace(&mut self.state, ConnectionState::Failed);
        if let ConnectionState::InTx { tx, .. } = state {
            tx.rollback();
        }
        // A Streaming cursor or an InTx cursor drops with `state`.
    }

    fn fail(&mut self, err: BoltError) -> Vec<ServerMessage> {
        self.discard_work();
        self.state = ConnectionState::Failed;
        vec![ServerMessage::Failure(err)]
    }

    fn handle_connected(&mut self, msg: ClientMessage) -> Vec<ServerMessage> {
        match msg {
            ClientMessage::Hello(hello) => match self.authenticator.authenticate(&hello.auth) {
                Ok(identity) => {
                    self.identity = Some(identity);
                    self.state = ConnectionState::Ready;
                    vec![hello_success(&self.connection_id)]
                }
                Err(err) => {
                    self.state = ConnectionState::Defunct;
                    vec![ServerMessage::Failure(err)]
                }
            },
            ClientMessage::Goodbye => {
                self.state = ConnectionState::Defunct;
                Vec::new()
            }
            _ => {
                self.state = ConnectionState::Defunct;
                vec![ServerMessage::Failure(BoltError::Invalid(
                    "the first message must be HELLO".into(),
                ))]
            }
        }
    }

    fn handle_failed(&mut self, msg: ClientMessage) -> Vec<ServerMessage> {
        match msg {
            ClientMessage::Reset => {
                self.pending_failure = None;
                self.interrupted.store(false, Ordering::SeqCst);
                self.state = ConnectionState::Ready;
                vec![reset_success()]
            }
            ClientMessage::Goodbye => {
                self.state = ConnectionState::Defunct;
                Vec::new()
            }
            _ => match self.pending_failure.take() {
                Some(err) => vec![ServerMessage::Failure(err)],
                None => vec![ServerMessage::Ignored],
            },
        }
    }

    fn handle_active(&mut self, msg: ClientMessage, now: Instant) -> Vec<ServerMessage> {
        if let ConnectionState::InTx { last_activity, .. }
        | ConnectionState::Streaming { last_activity, .. } = &mut self.state
        {
            *last_activity = now;
        }
        match msg {
            ClientMessage::Goodbye => {
                self.discard_work();
                self.state = ConnectionState::Defunct;
                Vec::new()
            }
            ClientMessage::Reset => {
                self.discard_work();
                self.interrupted.store(false, Ordering::SeqCst);
                self.state = ConnectionState::Ready;
                vec![reset_success()]
            }
            ClientMessage::Hello(_) => self.violate(BoltError::Invalid(format!(
                "HELLO is not allowed in state {}",
                self.state.name()
            ))),
            ClientMessage::Other(MessageKind::Route, fields) if self.allows(MessageKind::Route) => {
                self.handle_route(&fields)
            }
            ClientMessage::Other(kind, fields) => {
                if !self.allows(kind) {
                    return self.violate(BoltError::Invalid(format!(
                        "{kind:?} is not allowed in state {}",
                        self.state.name()
                    )));
                }
                match kind {
                    MessageKind::Begin => self.handle_begin(fields),
                    MessageKind::Commit => self.handle_commit(&fields),
                    MessageKind::Rollback => self.handle_rollback(&fields),
                    MessageKind::Run => self.handle_run(fields),
                    MessageKind::Pull => self.handle_stream(fields, false),
                    MessageKind::Discard => self.handle_stream(fields, true),
                    _ => self.fail(BoltError::NotSupported(format!(
                        "{kind:?} is not supported yet"
                    ))),
                }
            }
        }
    }

    /// `ROUTE`: answers from the routing config and stays in `Ready`. A
    /// malformed request is a protocol violation; an unknown database fails.
    fn handle_route(&mut self, fields: &[PackValue]) -> Vec<ServerMessage> {
        let Some(routing) = self.routing.clone() else {
            return self.fail(BoltError::NotSupported(
                "ROUTE is not supported on this listener".into(),
            ));
        };
        let requested = match requested_database(fields) {
            Ok(requested) => requested,
            Err(err) => return self.violate(err),
        };
        match routing.route(requested.as_deref()) {
            Ok(reply) => vec![reply],
            Err(err) => self.fail(err),
        }
    }

    /// The answer to a request cut short by a `RESET` that arrived while it ran:
    /// `IGNORED`, with the connection interrupted until the `RESET` is handled.
    /// `None` when no `RESET` is waiting, so the caller reports its own error.
    fn ignored_if_interrupted(&mut self) -> Option<Vec<ServerMessage>> {
        if !self.interrupted.load(Ordering::SeqCst) {
            return None;
        }
        self.interrupt();
        Some(vec![ServerMessage::Ignored])
    }

    /// `BEGIN`: waits for the request's bookmarks, then opens the transaction.
    /// A malformed request is a protocol violation; an unknown database, a
    /// bookmark timeout or a user to impersonate fails.
    fn handle_begin(&mut self, fields: Vec<PackValue>) -> Vec<ServerMessage> {
        let (Some(service), Some(identity)) = (self.service.clone(), self.identity.clone()) else {
            return self.fail(BoltError::NotSupported(
                "BEGIN is not supported on a connection with no database".into(),
            ));
        };
        let request = match BeginRequest::from_fields(fields) {
            Ok(Ok(request)) => request,
            Ok(Err(err)) => return self.fail(err),
            Err(err) => return self.violate(err),
        };
        // The database is fixed here, for every statement of the transaction.
        if let Err(err) = select_database(self.routing.as_ref(), request.database.as_deref()) {
            return self.fail(err);
        }
        let tx = match service.begin(&identity, &request) {
            Ok(tx) => tx,
            Err(err) => {
                return self
                    .ignored_if_interrupted()
                    .unwrap_or_else(|| self.fail(err))
            }
        };
        self.last_qid = -1;
        match self.enter_tx(tx, Instant::now()) {
            Ok(()) => vec![ServerMessage::Success(Vec::new())],
            Err(tx) => {
                tx.rollback();
                self.violate(BoltError::Invalid("BEGIN needs the Ready state".into()))
            }
        }
    }

    /// Takes the open transaction out of the state, leaving `Failed` so that
    /// a transaction that cannot finish ends the way a failed request does.
    fn take_tx(&mut self) -> Option<Box<dyn OpenTransaction>> {
        match std::mem::replace(&mut self.state, ConnectionState::Failed) {
            ConnectionState::InTx { tx, .. } => Some(tx),
            other => {
                self.state = other;
                None
            }
        }
    }

    /// `COMMIT`: one durable commit, replying with its bookmark. A conflict is
    /// a transient `FAILURE` (the transaction is gone either way). Open
    /// results are not allowed (`allows`): the state machine has `COMMIT` in
    /// `TX_READY` only, so a client pulls or discards first, as the drivers do.
    fn handle_commit(&mut self, fields: &[PackValue]) -> Vec<ServerMessage> {
        if !fields.is_empty() {
            return self.violate(BoltError::Invalid("COMMIT takes no fields".into()));
        }
        let Some(tx) = self.take_tx() else {
            return self.violate(BoltError::Invalid("COMMIT needs a transaction".into()));
        };
        match tx.commit() {
            Ok(seq) => {
                self.state = ConnectionState::Ready;
                vec![ServerMessage::Success(vec![(
                    "bookmark".to_string(),
                    PackValue::String(format_bookmark(seq)),
                )])]
            }
            Err(err) => vec![ServerMessage::Failure(err)],
        }
    }

    /// `ROLLBACK`: drops the buffered writes.
    fn handle_rollback(&mut self, fields: &[PackValue]) -> Vec<ServerMessage> {
        if !fields.is_empty() {
            return self.violate(BoltError::Invalid("ROLLBACK takes no fields".into()));
        }
        let Some(tx) = self.take_tx() else {
            return self.violate(BoltError::Invalid("ROLLBACK needs a transaction".into()));
        };
        tx.rollback();
        self.state = ConnectionState::Ready;
        vec![ServerMessage::Success(Vec::new())]
    }

    fn handle_run(&mut self, fields: Vec<PackValue>) -> Vec<ServerMessage> {
        let (Some(service), Some(identity)) = (self.service.clone(), self.identity.clone()) else {
            return self.fail(BoltError::NotSupported(
                "RUN is not supported on a connection with no database".into(),
            ));
        };
        let request = match RunRequest::from_fields(fields) {
            Ok(request) => request,
            Err(err) => return self.violate(err),
        };
        if let Err(err) = select_database(self.routing.as_ref(), request.database.as_deref()) {
            return self.fail(err);
        }
        let started = Instant::now();
        let in_tx = matches!(self.state, ConnectionState::InTx { .. });
        let opened = match &mut self.state {
            ConnectionState::Ready => service.run_auto_commit(&identity, &request),
            ConnectionState::InTx { tx, .. } => service.run_in_tx(&identity, tx.as_mut(), &request),
            _ => unreachable!("RUN is checked against the state before it is handled"),
        };
        let cursor = match opened {
            Ok(cursor) => cursor,
            Err(err) => {
                return self
                    .ignored_if_interrupted()
                    .unwrap_or_else(|| self.fail(err))
            }
        };
        let fields = cursor
            .fields()
            .iter()
            .map(|name| PackValue::String(name.clone()))
            .collect();
        let mut meta = vec![
            ("fields".to_string(), PackValue::List(fields)),
            (
                "t_first".to_string(),
                PackValue::Int(elapsed_millis(started)),
            ),
        ];
        if in_tx {
            self.last_qid += 1;
            let qid = self.last_qid;
            meta.push(("qid".to_string(), PackValue::Int(qid)));
            if let ConnectionState::InTx { cursors, .. } = &mut self.state {
                cursors.insert(qid, cursor);
            }
        } else {
            self.state = ConnectionState::Streaming {
                cursor,
                last_activity: Instant::now(),
            };
        }
        vec![ServerMessage::Success(meta)]
    }

    /// `PULL` (`discard` false) and `DISCARD` (true).
    ///
    /// In a transaction `qid` names one of the open results and `-1` the latest
    /// one. A `qid` that is not open is a request fault: a `FAILURE`, which per
    /// the Bolt state machine moves the connection to `FAILED`, so the
    /// transaction is rolled back like it is for any other failed request.
    fn handle_stream(&mut self, fields: Vec<PackValue>, discard: bool) -> Vec<ServerMessage> {
        let request = match StreamRequest::from_fields(fields) {
            Ok(Ok(request)) => request,
            Ok(Err(err)) => return self.fail(err),
            Err(err) => return self.violate(err),
        };
        let version = self.version;
        let mut tx_qid = None;
        let cursor = match &mut self.state {
            ConnectionState::Streaming { cursor, .. } if request.qid == -1 => Some(cursor),
            ConnectionState::InTx { cursors, .. } => {
                let qid = if request.qid == -1 {
                    cursors.keys().next_back().copied()
                } else {
                    Some(request.qid)
                };
                qid.and_then(|qid| {
                    tx_qid = Some(qid);
                    cursors.get_mut(&qid)
                })
            }
            _ => None,
        };
        let Some(cursor) = cursor else {
            return self.fail(BoltError::Invalid(format!(
                "no open result has qid {}",
                request.qid
            )));
        };
        let mut out = Vec::new();
        if discard {
            cursor.discard(request.limit);
        } else {
            match cursor.take(request.limit, version) {
                Ok(rows) => out.extend(rows.into_iter().map(ServerMessage::Record)),
                Err(err) => return self.fail(err),
            }
        }
        if cursor.has_more() {
            out.push(ServerMessage::Success(vec![(
                "has_more".to_string(),
                PackValue::Bool(true),
            )]));
            return out;
        }
        let meta = cursor.summary();
        match (&mut self.state, tx_qid) {
            (ConnectionState::InTx { cursors, .. }, Some(qid)) => {
                cursors.remove(&qid);
            }
            (state, _) => *state = ConnectionState::Ready,
        }
        out.push(ServerMessage::Success(meta));
        out
    }

    /// Whether the Bolt state machine accepts `kind` in the current state.
    fn allows(&self, kind: MessageKind) -> bool {
        use MessageKind::*;
        match &self.state {
            ConnectionState::Ready => matches!(kind, Run | Begin | Route),
            ConnectionState::Streaming { .. } => matches!(kind, Pull | Discard),
            ConnectionState::InTx { cursors, .. } if cursors.is_empty() => {
                matches!(kind, Run | Commit | Rollback)
            }
            ConnectionState::InTx { .. } => matches!(kind, Run | Pull | Discard),
            _ => false,
        }
    }
}

/// Whether a `RUN`'s `db` selects the store: the routing config's rule, which
/// is also `ROUTE`'s. A connection with no routing config does not know the
/// store's name, so it accepts only an absent or empty one.
fn select_database(
    routing: Option<&RoutingConfig>,
    requested: Option<&str>,
) -> Result<(), BoltError> {
    match (routing, requested) {
        (Some(routing), requested) => routing.check_database(requested),
        (None, None | Some("")) => Ok(()),
        (None, Some(name)) => Err(BoltError::DatabaseNotFound(format!(
            "database {name:?} does not exist"
        ))),
    }
}

fn elapsed_millis(since: Instant) -> i64 {
    i64::try_from(since.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// Settings for [`serve_connection`].
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub tx_timeout: Duration,
    /// How long a client has, from connecting, to finish the handshake and send
    /// a `HELLO` that authenticates.
    pub hello_timeout: Duration,
    /// How long a partly received message may stay unfinished. A client that
    /// sends nothing, or only keep-alives, between messages is not held to it.
    pub message_timeout: Duration,
    /// How long a write to the client may block before the connection is
    /// dropped.
    pub write_timeout: Duration,
    /// Largest request message once the client has authenticated.
    pub max_message_bytes: usize,
    /// Largest request message before `HELLO` succeeds. Small: nothing an
    /// unauthenticated client sends needs more, and each connection buffers up
    /// to this much.
    pub pre_auth_max_message_bytes: usize,
    /// Answers `ROUTE`. `None` leaves `ROUTE` unsupported.
    pub routing: Option<RoutingConfig>,
    /// How long a statement waits for the store to reach a bookmark.
    pub bookmark_timeout: Duration,
    /// The server's stop signal. Once it reads `true`, or its sender is
    /// dropped, the connection closes at its next idle point: while it waits
    /// for the handshake or for the client's next message, or right after it
    /// has sent the replies to the batch it was running. Closing drops the
    /// connection's state, which rolls back an open transaction. `None` never
    /// stops.
    pub shutdown: Option<tokio::sync::watch::Receiver<bool>>,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            tx_timeout: DEFAULT_TX_TIMEOUT,
            hello_timeout: Duration::from_secs(10),
            message_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(30),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            pre_auth_max_message_bytes: DEFAULT_PRE_AUTH_MAX_MESSAGE_BYTES,
            routing: None,
            bookmark_timeout: DEFAULT_BOOKMARK_TIMEOUT,
            shutdown: None,
        }
    }
}

/// Whether the stop signal has been given (or its sender is gone).
fn stopping(shutdown: &Option<tokio::sync::watch::Receiver<bool>>) -> bool {
    shutdown
        .as_ref()
        .is_some_and(|rx| *rx.borrow() || rx.has_changed().is_err())
}

/// Resolves once the stop signal is given; never, without one.
pub(crate) async fn stop_signalled(shutdown: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    match shutdown {
        // An error means the sender is gone, which counts as the signal.
        Some(rx) => {
            let _ = rx.wait_for(|stop| *stop).await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// How often the read loop wakes with nothing to read, to expire a transaction
/// and check its deadlines.
const TICK: Duration = Duration::from_secs(1);

/// Length of the run of `0x0000` chunks at the front of `buf`.
///
/// `framing::deframe` skips leading no-op chunks but reports `Incomplete`
/// when the buffer holds nothing else, because an empty message and a no-op are
/// the same two bytes. The buffer always starts at a message boundary, where a
/// zero chunk can only be a keep-alive, so they are consumed here: a client
/// that sends only keep-alives neither grows the buffer nor is waited on.
pub(crate) fn keepalive_len(buf: &[u8]) -> usize {
    buf.chunks_exact(2)
        .take_while(|pair| pair[0] == 0 && pair[1] == 0)
        .count()
        * 2
}

/// Why the read loop stopped without the client saying `GOODBYE`.
fn timed_out(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, what.to_string())
}

/// Runs one client connection to the end: handshake, then the message loop.
/// Returns `Ok` when the client says `GOODBYE`, disconnects, or the connection
/// becomes defunct, and a `TimedOut` error when a deadline passes.
pub async fn serve_connection<S>(
    stream: S,
    authenticator: Arc<TokenAuthenticator>,
    connection_id: String,
    config: ConnectionConfig,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_connection_with(stream, authenticator, connection_id, config, None).await
}

/// [`serve_connection`] with a store for `RUN` to run against; the database
/// names `RUN` accepts come from `config.routing`, as `ROUTE`'s do. Messages are
/// then handled on a blocking thread, because a statement can wait on a write
/// lock, an fsync or a bookmark.
pub async fn serve_connection_with<S>(
    mut stream: S,
    authenticator: Arc<TokenAuthenticator>,
    connection_id: String,
    config: ConnectionConfig,
    database: Option<SharedDb>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Tokio's clock, so a paused test clock drives every deadline here.
    let started = tokio::time::Instant::now();
    let mut shutdown = config.shutdown.clone();
    let mut request = [0u8; handshake::HANDSHAKE_LEN];
    let read = tokio::time::timeout(config.hello_timeout, stream.read_exact(&mut request));
    let read = tokio::select! {
        read = read => read,
        _ = stop_signalled(&mut shutdown) => return Ok(()),
    };
    match read {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => return Err(err),
        Err(_) => return Err(timed_out("handshake not received in time")),
    }
    let Ok(offer) = handshake::parse_request(&request) else {
        return Ok(());
    };
    let agreed = handshake::negotiate(offer);
    send_bytes(
        &mut stream,
        &handshake::response(agreed),
        config.write_timeout,
    )
    .await?;
    let Some(version) = agreed else {
        return Ok(());
    };

    let blocking = database.is_some();
    let mut connection = Connection::new(authenticator, config.tx_timeout, connection_id, version)
        .with_routing(config.routing.clone());
    if let Some(database) = database {
        connection = connection.with_database(database, config.bookmark_timeout);
    }
    let interrupt = connection.interrupt_flag();
    let mut buf: Vec<u8> = Vec::new();
    // Bytes read while a batch ran are in `buf` and have not been decoded.
    let mut unprocessed = false;
    // When the bytes now in `buf` began to wait for the rest of their message.
    let mut partial_since: Option<tokio::time::Instant> = None;
    loop {
        if stopping(&shutdown) {
            // Dropping `connection` rolls back an open transaction.
            let _ = stream.shutdown().await;
            return Ok(());
        }
        let authenticated = connection.identity().is_some();
        if !authenticated && started.elapsed() >= config.hello_timeout {
            return Err(timed_out("HELLO not received in time"));
        }
        if partial_since.is_some_and(|since| since.elapsed() >= config.message_timeout) {
            return Err(timed_out("message not completed in time"));
        }
        if std::mem::take(&mut unprocessed) {
            // The read already happened, while the last batch ran.
        } else {
            let read = tokio::select! {
                read = tokio::time::timeout(TICK, stream.read_buf(&mut buf)) => read,
                _ = stop_signalled(&mut shutdown) => continue,
            };
            match read {
                Ok(Ok(0)) => return Ok(()),
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    connection.check_timeout(Instant::now());
                    continue;
                }
            }
        }

        let cap = if connection.identity().is_some() {
            config.max_message_bytes
        } else {
            config.pre_auth_max_message_bytes
        };
        // Parse every complete message, advancing a cursor; the buffer is
        // drained once at the end, not once per message.
        let mut decoded: Vec<Result<ClientMessage, BoltError>> = Vec::new();
        let mut pos = 0;
        let mut oversized = false;
        loop {
            pos += keepalive_len(&buf[pos..]);
            match framing::deframe(&buf[pos..]) {
                Ok((message, used)) => {
                    pos += used;
                    if message.len() > cap {
                        oversized = true;
                        break;
                    }
                    decoded.push(ClientMessage::decode(&message));
                }
                Err(framing::FrameError::Incomplete) => break,
            }
        }
        buf.drain(..pos);
        if oversized || buf.len() > cap {
            let failure =
                ServerMessage::Failure(BoltError::TooLarge("request message is too large".into()));
            send(&mut stream, &[failure], config.write_timeout).await?;
            return Ok(());
        }
        // The clock runs from when the unfinished message's first bytes
        // arrived; a message that completed this round starts the next one's.
        partial_since = if buf.is_empty() {
            None
        } else if pos > 0 {
            Some(tokio::time::Instant::now())
        } else {
            partial_since.or_else(|| Some(tokio::time::Instant::now()))
        };

        // A RESET behind other requests interrupts them.
        if decoded
            .iter()
            .any(|m| matches!(m, Ok(ClientMessage::Reset)))
        {
            connection.interrupt();
        }
        let (returned, responses) = if blocking {
            let mut task = tokio::task::spawn_blocking(move || {
                let responses = handle_batch(&mut connection, decoded);
                (connection, responses)
            });
            // Keep reading while the batch runs, so a RESET (or a hang-up) is
            // seen at once. The bytes stay in `buf`, to be decoded after.
            let mut scanned = 0;
            let mut peer_gone: Option<std::io::Result<()>> = None;
            let joined = loop {
                tokio::select! {
                    joined = &mut task => break joined,
                    read = stream.read_buf(&mut buf), if peer_gone.is_none() && buf.len() <= cap => {
                        match read {
                            Ok(0) => peer_gone = Some(Ok(())),
                            Ok(_) => {
                                unprocessed = true;
                                if holds_reset(&buf, &mut scanned) {
                                    interrupt.store(true, Ordering::SeqCst);
                                }
                            }
                            Err(err) => peer_gone = Some(Err(err)),
                        }
                        if peer_gone.is_some() {
                            interrupt.store(true, Ordering::SeqCst);
                        }
                    }
                }
            };
            let joined = joined.map_err(std::io::Error::other)?;
            if let Some(ended) = peer_gone {
                // Nobody is listening for the replies; dropping the connection
                // rolls back an open transaction.
                return ended;
            }
            joined
        } else {
            let responses = handle_batch(&mut connection, decoded);
            (connection, responses)
        };
        connection = returned;
        if unprocessed && partial_since.is_none() {
            partial_since = Some(tokio::time::Instant::now());
        }
        send(&mut stream, &responses, config.write_timeout).await?;
        if connection.is_defunct() {
            let _ = stream.shutdown().await;
            return Ok(());
        }
    }
}

/// Whether the complete messages in `buf` from `scanned` on include a `RESET`
/// (an empty structure with tag `0x0F`). Advances `scanned` past the messages
/// it has looked at, so each is looked at once.
fn holds_reset(buf: &[u8], scanned: &mut usize) -> bool {
    const RESET_MESSAGE: [u8; 2] = [0xB0, 0x0F];
    let mut found = false;
    loop {
        let from = *scanned + keepalive_len(&buf[*scanned..]);
        match framing::deframe(&buf[from..]) {
            Ok((message, used)) => {
                found |= message == RESET_MESSAGE;
                *scanned = from + used;
            }
            Err(framing::FrameError::Incomplete) => {
                *scanned = from;
                return found;
            }
        }
    }
}

fn handle_batch(
    connection: &mut Connection,
    decoded: Vec<Result<ClientMessage, BoltError>>,
) -> Vec<ServerMessage> {
    let mut responses = Vec::new();
    let interrupted = connection.interrupt_flag();
    for item in decoded {
        // A RESET read while this batch ran: what is left of it is ignored.
        if interrupted.load(Ordering::SeqCst) {
            connection.interrupt();
        }
        responses.extend(match item {
            Ok(message) => connection.handle(message),
            Err(err) => connection.handle_malformed(err),
        });
    }
    responses
}

async fn send<S: AsyncWrite + Unpin>(
    stream: &mut S,
    responses: &[ServerMessage],
    write_timeout: Duration,
) -> std::io::Result<()> {
    if responses.is_empty() {
        return Ok(());
    }
    let mut out = Vec::new();
    for response in responses {
        framing::frame(&response.encode_or_failure(), &mut out);
    }
    send_bytes(stream, &out, write_timeout).await
}

/// Writes and flushes within `write_timeout`, so a client that never reads
/// cannot hold the task.
async fn send_bytes<S: AsyncWrite + Unpin>(
    stream: &mut S,
    bytes: &[u8],
    write_timeout: Duration,
) -> std::io::Result<()> {
    let write = async {
        stream.write_all(bytes).await?;
        stream.flush().await
    };
    match tokio::time::timeout(write_timeout, write).await {
        Ok(result) => result,
        Err(_) => Err(timed_out("write to client timed out")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keepalive_len_counts_only_leading_zero_chunks() {
        assert_eq!(keepalive_len(&[0, 0, 0, 0, 0, 1, 7, 0, 0]), 4);
        assert_eq!(keepalive_len(&[0, 0, 0, 0, 0]), 4);
        assert_eq!(keepalive_len(&[0, 1, 7, 0, 0]), 0);
        assert_eq!(keepalive_len(&[]), 0);
    }
}
