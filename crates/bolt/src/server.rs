//! The Bolt TCP listener: accepts sockets, optionally wraps them in TLS, and
//! runs one [`serve_connection_with`] task per client.
//!
//! - **Admission.** At most [`BoltConfig::max_connections`] clients are served
//!   at once. A client over the limit is refused cleanly: it gets its
//!   handshake answered, then a transient `FAILURE` to its first message
//!   (drivers retry a transient error), and the socket is closed. Refusals are
//!   bounded too ([`REFUSAL_SLOTS`] at a time); past that a socket is closed
//!   unanswered.
//! - **Deadlines.** A client must finish the handshake (TLS included) and an
//!   authenticating `HELLO` within the connection's `hello_timeout`, or it is
//!   dropped.
//! - **Isolation.** Every connection is its own task. A panic in one is logged
//!   and ends that connection only; the listener keeps accepting.
//! - **Shutdown.** When the stop signal is given, the listener stops accepting
//!   at once and every connection closes at its next idle point (see
//!   [`ConnectionConfig::shutdown`]), which rolls back an open transaction.
//!   Connections still open after [`BoltConfig::drain_timeout`] are aborted,
//!   and [`BoltListener::serve`] returns.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mushroomdb::SharedDb;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;

use crate::connection::{keepalive_len, serve_connection_with, stop_signalled, ConnectionConfig};
use crate::error::BoltError;
use crate::framing;
use crate::handshake;
use crate::messages::hello::TokenAuthenticator;
use crate::messages::ServerMessage;

/// Default for [`BoltConfig::max_connections`].
pub const DEFAULT_MAX_CONNECTIONS: usize = 256;

/// Default for [`BoltConfig::drain_timeout`].
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(25);

/// How many over-the-limit clients may be in the middle of being refused at
/// once. Past this a socket is closed without a reply.
pub const REFUSAL_SLOTS: usize = 16;

/// The longest a refusal may take, handshake included.
const REFUSAL_DEADLINE: Duration = Duration::from_secs(5);

/// The largest first message a refusal reads before it answers.
const REFUSAL_READ_CAP: usize = 64 * 1024;

/// How long the accept loop pauses after a failed `accept` (out of file
/// descriptors, say), so a persistent failure does not spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// Live connection counters of one listener, shared with whoever reports
/// them. Clones share the same counters.
#[derive(Debug, Clone, Default)]
pub struct BoltStats {
    /// Connections being served now (refused clients are not counted).
    pub open: Arc<AtomicU64>,
    /// Connections admitted since the listener started.
    pub accepted: Arc<AtomicU64>,
}

/// Counts one admitted connection as open until dropped, however its task ends.
struct OpenGuard(Arc<AtomicU64>);

impl OpenGuard {
    fn new(open: &Arc<AtomicU64>) -> Self {
        open.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(open))
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Listener settings.
#[derive(Debug, Clone)]
pub struct BoltConfig {
    /// Per-connection settings: deadlines, limits, the routing config that
    /// answers `ROUTE` and selects the database. The listener sets
    /// `shutdown` itself.
    pub connection: ConnectionConfig,
    /// Clients served at once.
    pub max_connections: usize,
    /// After the stop signal, how long connections have to close before they
    /// are aborted.
    pub drain_timeout: Duration,
    /// Counters the listener keeps current; clone one before `serve` to read it.
    pub stats: BoltStats,
    /// Serve Bolt over TLS with this config. `None` is plaintext.
    #[cfg(feature = "tls")]
    pub tls: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
}

impl Default for BoltConfig {
    fn default() -> Self {
        Self {
            connection: ConnectionConfig::default(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
            stats: BoltStats::default(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

impl BoltConfig {
    /// Whether connections are served over TLS.
    pub fn is_tls(&self) -> bool {
        #[cfg(feature = "tls")]
        {
            self.tls.is_some()
        }
        #[cfg(not(feature = "tls"))]
        {
            false
        }
    }
}

/// Whether the accept loop serves a socket or refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Served, with this connection number.
    Accepted(u64),
    /// Over the connection limit, which is carried for the message.
    Refused { limit: usize },
}

/// A bound Bolt listener. Binding and serving are separate so a caller can
/// learn the bound address (port 0 picks a free one) before it builds the
/// routing config that advertises it.
pub struct BoltListener {
    listener: TcpListener,
}

impl BoltListener {
    pub async fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(addr).await?,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts clients until `shutdown` reads `true` (or its sender is
    /// dropped), then drains as the module docs describe. Returns `Ok` once
    /// every connection has ended or been aborted.
    pub async fn serve(
        self,
        db: SharedDb,
        authenticator: Arc<TokenAuthenticator>,
        config: BoltConfig,
        shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        let mut connection = config.connection.clone();
        connection.shutdown = Some(shutdown.clone());
        #[cfg(feature = "tls")]
        let acceptor = config.tls.clone().map(tokio_rustls::TlsAcceptor::from);
        let handler = move |socket: TcpStream, admission: Admission| {
            let db = db.clone();
            let authenticator = authenticator.clone();
            let connection = connection.clone();
            #[cfg(feature = "tls")]
            let acceptor = acceptor.clone();
            async move {
                #[cfg(feature = "tls")]
                if let Some(acceptor) = acceptor {
                    let mut stop = connection.shutdown.clone();
                    // The TLS handshake gets its own `hello_timeout`; the
                    // Bolt handshake and `HELLO` then get theirs.
                    let accept =
                        tokio::time::timeout(connection.hello_timeout, acceptor.accept(socket));
                    let tls = tokio::select! {
                        tls = accept => tls,
                        _ = stop_signalled(&mut stop) => return Ok(()),
                    };
                    let tls = match tls {
                        Ok(Ok(tls)) => tls,
                        Ok(Err(err)) => return Err(err),
                        Err(_) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "TLS handshake not completed in time",
                            ))
                        }
                    };
                    return run_admitted(tls, admission, db, authenticator, connection).await;
                }
                run_admitted(socket, admission, db, authenticator, connection).await
            }
        };
        accept_loop_with_stats(
            self.listener,
            config.max_connections,
            config.drain_timeout,
            shutdown,
            handler,
            config.stats.clone(),
        )
        .await
    }
}

/// Serves or refuses one client on an established stream.
async fn run_admitted<S>(
    stream: S,
    admission: Admission,
    db: SharedDb,
    authenticator: Arc<TokenAuthenticator>,
    connection: ConnectionConfig,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match admission {
        Admission::Accepted(number) => {
            serve_connection_with(
                stream,
                authenticator,
                format!("bolt-{number}"),
                connection,
                Some(db),
            )
            .await
        }
        Admission::Refused { limit } => {
            refuse(stream, limit).await;
            Ok(())
        }
    }
}

/// The accept loop, with the per-socket work supplied by `handler` so the
/// admission, isolation and drain rules can be tested on their own.
#[cfg(test)]
pub(crate) async fn accept_loop<H, F>(
    listener: TcpListener,
    max_connections: usize,
    drain_timeout: Duration,
    shutdown: watch::Receiver<bool>,
    handler: H,
) -> std::io::Result<()>
where
    H: Fn(TcpStream, Admission) -> F,
    F: Future<Output = std::io::Result<()>> + Send + 'static,
{
    accept_loop_with_stats(
        listener,
        max_connections,
        drain_timeout,
        shutdown,
        handler,
        BoltStats::default(),
    )
    .await
}

/// [`accept_loop`] that keeps `stats` current.
pub(crate) async fn accept_loop_with_stats<H, F>(
    listener: TcpListener,
    max_connections: usize,
    drain_timeout: Duration,
    shutdown: watch::Receiver<bool>,
    handler: H,
    stats: BoltStats,
) -> std::io::Result<()>
where
    H: Fn(TcpStream, Admission) -> F,
    F: Future<Output = std::io::Result<()>> + Send + 'static,
{
    let permits = Arc::new(Semaphore::new(max_connections));
    let refusals = Arc::new(Semaphore::new(REFUSAL_SLOTS));
    let mut tasks: JoinSet<std::io::Result<()>> = JoinSet::new();
    let mut stop = Some(shutdown);
    let mut accepted: u64 = 0;
    loop {
        tokio::select! {
            _ = stop_signalled(&mut stop) => break,
            Some(done) = tasks.join_next(), if !tasks.is_empty() => log_ended(done),
            next = listener.accept() => {
                let socket = match next {
                    Ok((socket, _peer)) => socket,
                    Err(err) => {
                        tracing::warn!(error = %err, "bolt: accept failed");
                        tokio::time::sleep(ACCEPT_RETRY).await;
                        continue;
                    }
                };
                let _ = socket.set_nodelay(true);
                match permits.clone().try_acquire_owned() {
                    Ok(permit) => {
                        accepted += 1;
                        stats.accepted.store(accepted, Ordering::Release);
                        let open = OpenGuard::new(&stats.open);
                        let work = handler(socket, Admission::Accepted(accepted));
                        tasks.spawn(async move {
                            let _permit = permit;
                            let _open = open;
                            work.await
                        });
                    }
                    Err(_) => match refusals.clone().try_acquire_owned() {
                        Ok(slot) => {
                            tracing::warn!(
                                limit = max_connections,
                                "bolt: connection limit reached; refusing a client"
                            );
                            let work = handler(socket, Admission::Refused { limit: max_connections });
                            tasks.spawn(async move {
                                let _slot = slot;
                                work.await
                            });
                        }
                        Err(_) => drop(socket),
                    },
                }
            }
        }
    }
    // Stop accepting before the drain: a client that connects now is refused
    // by the operating system rather than left waiting.
    drop(listener);
    let open = tasks.len();
    if open > 0 {
        tracing::info!(open, "bolt: stopped accepting; closing open connections");
    }
    let drained = tokio::time::timeout(drain_timeout, async {
        while let Some(done) = tasks.join_next().await {
            log_ended(done);
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            open = tasks.len(),
            drain_ms = u64::try_from(drain_timeout.as_millis()).unwrap_or(u64::MAX),
            "bolt: connections still open after the drain period; closing them"
        );
        tasks.shutdown().await;
    }
    Ok(())
}

/// Logs how a connection task ended. A panic is an error; a client that timed
/// out or went away is routine.
fn log_ended(done: Result<std::io::Result<()>, tokio::task::JoinError>) {
    match done {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::debug!(error = %err, "bolt: connection ended"),
        Err(err) if err.is_panic() => {
            tracing::error!(error = %err, "bolt: a connection task panicked; the listener carries on")
        }
        Err(_) => {}
    }
}

/// Refuses a client over the connection limit: answers the handshake, reads
/// its first message, answers it with a transient `FAILURE` and closes. Reading
/// the message first keeps the reply from being lost to a reset when the socket
/// closes with the client's bytes unread.
async fn refuse<S>(mut stream: S, limit: usize)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let refusal = async {
        let mut request = [0u8; handshake::HANDSHAKE_LEN];
        stream.read_exact(&mut request).await?;
        let Ok(offer) = handshake::parse_request(&request) else {
            return Ok(());
        };
        let agreed = handshake::negotiate(offer);
        stream.write_all(&handshake::response(agreed)).await?;
        stream.flush().await?;
        if agreed.is_none() {
            return Ok(());
        }
        let mut buf = Vec::new();
        loop {
            let at = keepalive_len(&buf);
            if framing::deframe(&buf[at..]).is_ok() || buf.len() > REFUSAL_READ_CAP {
                break;
            }
            if stream.read_buf(&mut buf).await? == 0 {
                return Ok(());
            }
        }
        let failure = ServerMessage::Failure(BoltError::Unavailable(format!(
            "the server is at its limit of {limit} Bolt connections; retry later"
        )));
        let mut out = Vec::new();
        framing::frame(&failure.encode_or_failure(), &mut out);
        stream.write_all(&out).await?;
        stream.flush().await?;
        stream.shutdown().await
    };
    let _: Result<std::io::Result<()>, _> = tokio::time::timeout(REFUSAL_DEADLINE, refusal).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn bound() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    /// Reads until the server closes, and returns what it sent.
    async fn read_to_close(addr: SocketAddr) -> Vec<u8> {
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut got = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut got))
            .await
            .expect("the server closes the socket")
            .ok();
        got
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_connection_does_not_stop_the_listener() {
        let (listener, addr) = bound().await;
        let (stop_tx, stop_rx) = watch::channel(false);
        let served = Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        let handler = move |mut socket: TcpStream, admission: Admission| {
            let counter = counter.clone();
            async move {
                let Admission::Accepted(number) = admission else {
                    return Ok(());
                };
                if number == 1 {
                    panic!("connection 1 panics");
                }
                counter.fetch_add(1, Ordering::SeqCst);
                socket.write_all(b"ok").await?;
                socket.shutdown().await
            }
        };
        let task = tokio::spawn(accept_loop(
            listener,
            4,
            Duration::from_secs(1),
            stop_rx,
            handler,
        ));
        // The first connection's task panics; the socket is dropped with it.
        assert_eq!(read_to_close(addr).await, b"");
        // The listener is still accepting, and the panicked task's slot is free.
        for _ in 0..4 {
            assert_eq!(read_to_close(addr).await, b"ok");
        }
        assert_eq!(served.load(Ordering::SeqCst), 4);
        stop_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stats_count_open_and_accepted_connections() {
        let (listener, addr) = bound().await;
        let (stop_tx, stop_rx) = watch::channel(false);
        let stats = BoltStats::default();
        let handler = |socket: TcpStream, _admission: Admission| async move {
            let mut socket = socket;
            let mut buf = [0u8; 1];
            let _ = socket.read(&mut buf).await;
            Ok(())
        };
        let task = tokio::spawn(accept_loop_with_stats(
            listener,
            4,
            Duration::from_secs(1),
            stop_rx,
            handler,
            stats.clone(),
        ));
        let a = TcpStream::connect(addr).await.unwrap();
        let b = TcpStream::connect(addr).await.unwrap();
        for _ in 0..100 {
            if stats.accepted.load(Ordering::Acquire) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(stats.accepted.load(Ordering::Acquire), 2);
        assert_eq!(stats.open.load(Ordering::Acquire), 2);
        drop(a);
        // The handler reads EOF, returns, and the slot is released.
        for _ in 0..100 {
            if stats.open.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(stats.open.load(Ordering::Acquire), 1);
        assert_eq!(stats.accepted.load(Ordering::Acquire), 2);
        drop(b);
        stop_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_drain_aborts_connections_that_outlive_it() {
        let (listener, addr) = bound().await;
        let (stop_tx, stop_rx) = watch::channel(false);
        // A connection that ignores the stop signal.
        let handler = |socket: TcpStream, _admission: Admission| async move {
            tokio::time::sleep(Duration::from_secs(600)).await;
            drop(socket);
            Ok(())
        };
        let task = tokio::spawn(accept_loop(
            listener,
            4,
            Duration::from_millis(300),
            stop_rx,
            handler,
        ));
        let mut client = TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("serve returns after the drain period")
            .unwrap()
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
        // The aborted task dropped its socket.
        let mut rest = Vec::new();
        let read =
            tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut rest)).await;
        assert!(read.is_ok(), "the socket is closed");
        // And nothing is listening any more.
        assert!(TcpStream::connect(addr).await.is_err());
    }
}
