//! The per-connection actor: a state machine over decoded messages, and the
//! read loop that feeds it.
//!
//! [`Connection`] holds no I/O. It takes one [`ClientMessage`] and returns the
//! responses to send, so every transition is testable without a socket.
//! [`serve_connection`] is the loop around it: handshake, deframe, dispatch,
//! write, plus the one-second tick that expires an idle transaction.
//!
//! States follow the Bolt 4.4/5.0 server state machine:
//! `NotAuthenticated` (the spec's CONNECTED), `Ready`, `Streaming`, `InTx`
//! (TX_READY and TX_STREAMING), `Failed`, `Interrupted` and `Defunct`. A message
//! the state does not allow fails the request and moves to `Failed`, where
//! everything but `RESET` and `GOODBYE` is `IGNORED`. A `RESET` that arrives
//! while earlier messages are still queued interrupts them: they are `IGNORED`
//! and the `RESET` returns the connection to `Ready`.
//!
//! Handlers for `RUN`, `PULL`, `DISCARD`, `BEGIN`, `COMMIT`, `ROLLBACK` and
//! `ROUTE` belong to later tasks. Until they land, those messages are checked
//! for legality in the current state and then answered with a not-supported
//! `FAILURE`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::BoltError;
use crate::framing;
use crate::handshake;
use crate::messages::hello::{hello_success, Identity, TokenAuthenticator};
use crate::messages::reset::reset_success;
use crate::messages::{ClientMessage, MessageKind, ServerMessage};

/// Default idle limit for an open transaction.
pub const DEFAULT_TX_TIMEOUT: Duration = Duration::from_secs(30);

/// Default limit on a single request message.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// An open transaction, as the connection sees it. Dropping it discards its
/// buffer; `rollback` is the explicit form.
pub trait OpenTransaction: Send {
    fn rollback(self: Box<Self>);
}

/// An open result cursor. Dropping it closes it.
pub trait ResultCursor: Send {}

/// Where a connection is in the protocol.
pub enum ConnectionState {
    /// Before `HELLO`.
    NotAuthenticated,
    Ready,
    /// An auto-commit result is open.
    Streaming {
        cursor: Box<dyn ResultCursor>,
    },
    /// Between `BEGIN` and `COMMIT`/`ROLLBACK`. `last_activity` is the last
    /// message received; the idle limit counts from it.
    InTx {
        tx: Box<dyn OpenTransaction>,
        opened_at: Instant,
        last_activity: Instant,
        cursor: Option<Box<dyn ResultCursor>>,
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
            ConnectionState::InTx { cursor: None, .. } => "InTx",
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
        }
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
            cursor: None,
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
        self.state = ConnectionState::Streaming { cursor };
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

    /// Rolls back a transaction that has been idle past the limit. Called on
    /// every message and by the one-second ticker. Returns whether it expired
    /// one; the client learns of it as the `FAILURE` to its next request.
    pub fn check_timeout(&mut self, now: Instant) -> bool {
        let expired = matches!(
            &self.state,
            ConnectionState::InTx { last_activity, .. }
                if now.saturating_duration_since(*last_activity) >= self.tx_timeout
        );
        if expired {
            self.discard_work();
            self.state = ConnectionState::Failed;
            self.pending_failure = Some(BoltError::TransactionTimedOut);
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

    /// A request that could not be decoded.
    pub fn handle_malformed(&mut self, err: BoltError) -> Vec<ServerMessage> {
        match self.state {
            ConnectionState::Defunct => Vec::new(),
            ConnectionState::NotAuthenticated => {
                self.state = ConnectionState::Defunct;
                vec![ServerMessage::Failure(err)]
            }
            ConnectionState::Failed | ConnectionState::Interrupted => {
                vec![ServerMessage::Ignored]
            }
            _ => self.fail(err),
        }
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
        if let ConnectionState::InTx { last_activity, .. } = &mut self.state {
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
                self.state = ConnectionState::Ready;
                vec![reset_success()]
            }
            ClientMessage::Hello(_) => self.fail(BoltError::Invalid(format!(
                "HELLO is not allowed in state {}",
                self.state.name()
            ))),
            ClientMessage::Other(kind, _) => {
                if self.allows(kind) {
                    self.fail(BoltError::NotSupported(format!(
                        "{kind:?} is not supported yet"
                    )))
                } else {
                    self.fail(BoltError::Invalid(format!(
                        "{kind:?} is not allowed in state {}",
                        self.state.name()
                    )))
                }
            }
        }
    }

    /// Whether the Bolt state machine accepts `kind` in the current state.
    fn allows(&self, kind: MessageKind) -> bool {
        use MessageKind::*;
        match &self.state {
            ConnectionState::Ready => matches!(kind, Run | Begin | Route),
            ConnectionState::Streaming { .. } => matches!(kind, Pull | Discard),
            ConnectionState::InTx { cursor: None, .. } => {
                matches!(kind, Run | Commit | Rollback)
            }
            ConnectionState::InTx {
                cursor: Some(_), ..
            } => {
                matches!(kind, Run | Pull | Discard)
            }
            _ => false,
        }
    }
}

/// Settings for [`serve_connection`].
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub tx_timeout: Duration,
    /// How long a client may take to send the 20-byte handshake.
    pub handshake_timeout: Duration,
    pub max_message_bytes: usize,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            tx_timeout: DEFAULT_TX_TIMEOUT,
            handshake_timeout: Duration::from_secs(10),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

/// How often the read loop wakes with nothing to read, to expire a transaction.
const TICK: Duration = Duration::from_secs(1);

/// Drops the `0x0000` chunks at the front of `buf`.
///
/// `framing::deframe` skips leading no-op chunks but reports `Incomplete`
/// when the buffer holds nothing else, because an empty message and a no-op are
/// the same two bytes. The buffer always starts at a message boundary, where a
/// zero chunk can only be a keep-alive, so they are consumed here: a client
/// that sends only keep-alives neither grows the buffer nor is waited on.
fn strip_keepalives(buf: &mut Vec<u8>) {
    let keep_from = buf
        .chunks_exact(2)
        .take_while(|pair| pair[0] == 0 && pair[1] == 0)
        .count()
        * 2;
    buf.drain(..keep_from);
}

/// Runs one client connection to the end: handshake, then the message loop.
/// Returns when the client says `GOODBYE`, disconnects, or the connection
/// becomes defunct.
pub async fn serve_connection<S>(
    mut stream: S,
    authenticator: Arc<TokenAuthenticator>,
    connection_id: String,
    config: ConnectionConfig,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut request = [0u8; handshake::HANDSHAKE_LEN];
    let read = tokio::time::timeout(config.handshake_timeout, stream.read_exact(&mut request));
    match read.await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => return Err(err),
        Err(_) => return Ok(()),
    }
    let Ok(offer) = handshake::parse_request(&request) else {
        return Ok(());
    };
    let agreed = handshake::negotiate(offer);
    stream.write_all(&handshake::response(agreed)).await?;
    stream.flush().await?;
    let Some(version) = agreed else {
        return Ok(());
    };

    let mut connection = Connection::new(authenticator, config.tx_timeout, connection_id, version);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match tokio::time::timeout(TICK, stream.read_buf(&mut buf)).await {
            Ok(Ok(0)) => return Ok(()),
            Ok(Ok(_)) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                connection.check_timeout(Instant::now());
                continue;
            }
        }

        let mut decoded: Vec<Result<ClientMessage, BoltError>> = Vec::new();
        loop {
            strip_keepalives(&mut buf);
            match framing::deframe(&buf) {
                Ok((message, used)) => {
                    buf.drain(..used);
                    decoded.push(ClientMessage::decode(&message));
                }
                Err(framing::FrameError::Incomplete) => break,
            }
        }
        if buf.len() > config.max_message_bytes {
            send(
                &mut stream,
                &[ServerMessage::Failure(BoltError::TooLarge(
                    "request message is too large".into(),
                ))],
            )
            .await?;
            return Ok(());
        }

        // A RESET behind other requests interrupts them.
        if decoded
            .iter()
            .any(|m| matches!(m, Ok(ClientMessage::Reset)))
        {
            connection.interrupt();
        }
        let mut responses = Vec::new();
        for item in decoded {
            responses.extend(match item {
                Ok(message) => connection.handle(message),
                Err(err) => connection.handle_malformed(err),
            });
        }
        send(&mut stream, &responses).await?;
        if connection.is_defunct() {
            let _ = stream.shutdown().await;
            return Ok(());
        }
    }
}

async fn send<S: AsyncWrite + Unpin>(
    stream: &mut S,
    responses: &[ServerMessage],
) -> std::io::Result<()> {
    if responses.is_empty() {
        return Ok(());
    }
    let mut out = Vec::new();
    for response in responses {
        framing::frame(&response.encode_or_failure(), &mut out);
    }
    stream.write_all(&out).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_keepalives_removes_only_leading_zero_chunks() {
        let mut buf = vec![0, 0, 0, 0, 0, 1, 7, 0, 0];
        strip_keepalives(&mut buf);
        assert_eq!(buf, vec![0, 1, 7, 0, 0]);
        let mut only = vec![0, 0, 0, 0, 0];
        strip_keepalives(&mut only);
        assert_eq!(only, vec![0]);
        let mut none: Vec<u8> = vec![];
        strip_keepalives(&mut none);
        assert!(none.is_empty());
    }
}
