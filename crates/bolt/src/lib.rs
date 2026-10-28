//! Bolt wire protocol for mushroomdb.
//!
//! `packstream` is the serialization format every Bolt message is built from,
//! `framing` and `handshake` carry it over a socket, `messages` decodes the
//! requests and encodes the responses, `connection` is the per-connection
//! state machine and read loop, and `server` is the TCP listener that runs one
//! connection per client.

pub mod connection;
pub mod error;
pub mod framing;
pub mod handshake;
pub mod messages;
pub mod packstream;
pub mod server;
