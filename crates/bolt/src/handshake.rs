//! Bolt version handshake.
//!
//! The client sends the magic preamble `0x6060B017` and four big-endian `u32`
//! version slots; the server answers with one `u32` naming the agreed version,
//! or `0` when none match.
//!
//! A slot is `[pad, range, minor, major]`, one byte each. It accepts server
//! version `major.v` for every `v` in `[minor - range, minor]`. The slot
//! `0x000001FF` is the manifest indicator (the Bolt 5.7+ manifest handshake);
//! this server does not implement the manifest exchange, so it skips that slot
//! and negotiates from the others. Every current driver sends it first and
//! follows it with ordinary range slots.
//!
//! Version set (audited 2026-10-20 against the five official drivers, see
//! `docs/site/bolt.md`): all five send `[0x000001FF, 5.8 range 8, 4.4 range 2,
//! 3.0]`, so 5.0 and 4.4 are the smallest set every driver negotiates. 5.1+
//! `LOGON`/`LOGOFF` is not required. Where each offer was read (default
//! branches, 2026-10-20; repository names are withheld by the names gate):
//! - Python 6.4.1: `Bolt.get_handshake` in `_sync/io/_bolt/_base.py`
//!   (`00 00 01 ff 00 08 08 05 00 02 04 04 00 00 00 03`)
//! - JavaScript 6.2.0: `newHandshakeBuffer` in `bolt-connection/src/bolt/handshake.js`
//! - Java 6.3.0: `HANDSHAKE_BUF` in the bolt-connection library's `BoltProtocolUtil.java`
//! - Go 6.3.0: `handshakeProposals` in `internal/bolt/connect.go`
//! - .NET 6.4.0: `PackSupportedVersions` in `Internal/Protocol/BoltProtocolFactory.cs`

/// First four bytes a client sends.
pub const MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

/// Total handshake request length: magic plus four version slots.
pub const HANDSHAKE_LEN: usize = 20;

/// The manifest indicator slot. Skipped, never matched.
pub const MANIFEST_SLOT: u32 = 0x0000_01FF;

/// Bolt versions this server speaks as `(major, minor)`, in preference order.
pub const SERVER_VERSIONS: [(u8, u8); 2] = [(5, 0), (4, 4)];

/// The server's offer as slots (`[pad, range, minor, major]`), padded with zeros.
pub const SERVER_OFFER: [u32; 4] = [0x0000_0005, 0x0000_0404, 0, 0];

/// Why a handshake request could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeError {
    /// Fewer than [`HANDSHAKE_LEN`] bytes were supplied.
    Incomplete,
    /// The first four bytes are not [`MAGIC`] (for example an HTTP request).
    BadMagic,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Incomplete => write!(f, "incomplete bolt handshake"),
            HandshakeError::BadMagic => write!(f, "not a bolt handshake: bad magic preamble"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// Reads the client's four version slots from a 20-byte handshake request.
pub fn parse_request(bytes: &[u8]) -> Result<[u32; 4], HandshakeError> {
    if bytes.len() < HANDSHAKE_LEN {
        return Err(HandshakeError::Incomplete);
    }
    if bytes[..4] != MAGIC {
        return Err(HandshakeError::BadMagic);
    }
    let mut slots = [0u32; 4];
    for (index, slot) in slots.iter_mut().enumerate() {
        let at = 4 + index * 4;
        *slot = u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    }
    Ok(slots)
}

/// Picks the agreed version: the first client slot (client preference order)
/// that accepts one of the server's versions, trying the server's versions in
/// its own preference order. Returns `(major, minor)`.
pub fn negotiate(client_offer: [u32; 4]) -> Option<(u8, u8)> {
    for slot in client_offer {
        if slot == MANIFEST_SLOT || slot == 0 {
            continue;
        }
        let range = ((slot >> 16) & 0xFF) as i16;
        let minor = ((slot >> 8) & 0xFF) as i16;
        let major = (slot & 0xFF) as u8;
        for (server_major, server_minor) in SERVER_VERSIONS {
            let server_minor_wide = i16::from(server_minor);
            if server_major == major
                && server_minor_wide <= minor
                && server_minor_wide >= minor - range
            {
                return Some((server_major, server_minor));
            }
        }
    }
    None
}

/// The four-byte reply: `[0, 0, minor, major]`, or all zeros for no match.
pub fn response(agreed: Option<(u8, u8)>) -> [u8; 4] {
    match agreed {
        Some((major, minor)) => [0, 0, minor, major],
        None => [0; 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a slot from its `[pad, range, minor, major]` bytes.
    fn slot(pad: u8, range: u8, minor: u8, major: u8) -> u32 {
        u32::from_be_bytes([pad, range, minor, major])
    }

    #[test]
    fn negotiate_5_0() {
        assert_eq!(negotiate([slot(0, 0, 0, 5), 0, 0, 0]), Some((5, 0)));
    }

    #[test]
    fn negotiate_4_4() {
        assert_eq!(negotiate([slot(0, 0, 4, 4), 0, 0, 0]), Some((4, 4)));
        assert_eq!(slot(0, 0, 4, 4), 0x0000_0404);
    }

    #[test]
    fn negotiate_range_match() {
        // Major 5 range 0-2 matches the server's 5.0.
        assert_eq!(negotiate([slot(0, 2, 0, 5), 0, 0, 0]), Some((5, 0)));
        // 5.8 with range 8 is 5.0-5.8.
        assert_eq!(negotiate([slot(0, 8, 8, 5), 0, 0, 0]), Some((5, 0)));
        // 4.4 with range 2 is 4.2-4.4.
        assert_eq!(negotiate([slot(0, 2, 4, 4), 0, 0, 0]), Some((4, 4)));
    }

    #[test]
    fn negotiate_range_excludes_outside_versions() {
        // 5.8 with range 7 is 5.1-5.8: no 5.0.
        assert_eq!(negotiate([slot(0, 7, 8, 5), 0, 0, 0]), None);
        // 4.3 with range 2 is 4.1-4.3: no 4.4.
        assert_eq!(negotiate([slot(0, 2, 3, 4), 0, 0, 0]), None);
        // Range wider than minor does not underflow.
        assert_eq!(negotiate([slot(0, 255, 1, 5), 0, 0, 0]), Some((5, 0)));
    }

    #[test]
    fn negotiate_manifest_slot_skipped() {
        assert_eq!(MANIFEST_SLOT, 0x0000_01FF);
        assert_eq!(
            negotiate([MANIFEST_SLOT, slot(0, 8, 8, 5), slot(0, 2, 4, 4), 3]),
            Some((5, 0))
        );
        // Only a manifest slot: nothing to negotiate.
        assert_eq!(negotiate([MANIFEST_SLOT, 0, 0, 0]), None);
    }

    #[test]
    fn negotiate_current_driver_offer() {
        // Python 6.4.1 sends 00 00 01 ff | 00 08 08 05 | 00 02 04 04 | 00 00 00 03.
        let bytes = [
            0x60, 0x60, 0xB0, 0x17, 0x00, 0x00, 0x01, 0xFF, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02,
            0x04, 0x04, 0x00, 0x00, 0x00, 0x03,
        ];
        let offer = parse_request(&bytes).unwrap();
        assert_eq!(offer, [0x0000_01FF, 0x0008_0805, 0x0002_0404, 0x0000_0003]);
        assert_eq!(negotiate(offer), Some((5, 0)));
        assert_eq!(response(negotiate(offer)), [0, 0, 0, 5]);
    }

    #[test]
    fn negotiate_client_preference_wins() {
        // Client lists 4.4 before 5.x: it gets 4.4.
        assert_eq!(
            negotiate([slot(0, 0, 4, 4), slot(0, 0, 0, 5), 0, 0]),
            Some((4, 4))
        );
    }

    #[test]
    fn negotiate_no_match() {
        assert_eq!(negotiate([slot(0, 0, 0, 3), 0, 0, 0]), None);
        assert_eq!(negotiate([0, 0, 0, 0]), None);
        assert_eq!(negotiate([slot(0, 0, 0, 6), slot(0, 0, 1, 5), 0, 0]), None);
    }

    #[test]
    fn server_offer_matches_server_versions() {
        let mut agreed = Vec::new();
        for slot_value in SERVER_OFFER {
            if slot_value != 0 {
                let major = (slot_value & 0xFF) as u8;
                let minor = ((slot_value >> 8) & 0xFF) as u8;
                agreed.push((major, minor));
            }
        }
        assert_eq!(agreed, SERVER_VERSIONS.to_vec());
    }

    #[test]
    fn response_bytes() {
        assert_eq!(response(Some((5, 0))), [0, 0, 0, 5]);
        assert_eq!(response(Some((4, 4))), [0, 0, 4, 4]);
        assert_eq!(response(None), [0, 0, 0, 0]);
    }

    #[test]
    fn parse_request_rejects_bad_input() {
        assert_eq!(
            parse_request(&[0x60, 0x60]),
            Err(HandshakeError::Incomplete)
        );
        let mut http = b"GET / HTTP/1.1\r\n\r\n....".to_vec();
        http.truncate(HANDSHAKE_LEN);
        assert_eq!(parse_request(&http), Err(HandshakeError::BadMagic));
    }
}
