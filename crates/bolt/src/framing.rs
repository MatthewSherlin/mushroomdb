//! Bolt chunked message framing.
//!
//! A message is sent as one or more chunks. Each chunk is a two-byte
//! big-endian length followed by that many bytes (at most 65535). The message
//! ends with a zero-length chunk, `0x0000`. A zero-length chunk with no message
//! bytes before it is a no-op keep-alive and carries no message.

/// Largest payload one chunk can carry.
pub const MAX_CHUNK: usize = u16::MAX as usize;

/// Why a byte stream could not be read as a framed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The stream ends before a complete message; read more bytes and retry.
    Incomplete,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Incomplete => write!(f, "incomplete bolt message: more bytes needed"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Appends `msg` to `buf` as chunks followed by the `0x0000` end marker.
pub fn frame(msg: &[u8], buf: &mut Vec<u8>) {
    for chunk in msg.chunks(MAX_CHUNK) {
        buf.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
        buf.extend_from_slice(chunk);
    }
    buf.extend_from_slice(&[0, 0]);
}

/// Reads one message from the front of `stream`.
///
/// Returns the reassembled message and the number of stream bytes consumed
/// (including any leading no-op chunks and the end marker). Returns
/// `FrameError::Incomplete` when the stream holds no complete message yet.
pub fn deframe(stream: &[u8]) -> Result<(Vec<u8>, usize), FrameError> {
    let mut message = Vec::new();
    let mut pos = 0;
    loop {
        let header = stream.get(pos..pos + 2).ok_or(FrameError::Incomplete)?;
        let len = u16::from_be_bytes([header[0], header[1]]) as usize;
        pos += 2;
        if len == 0 {
            if message.is_empty() {
                // No-op keep-alive between messages.
                continue;
            }
            return Ok((message, pos));
        }
        let body = stream.get(pos..pos + len).ok_or(FrameError::Incomplete)?;
        message.extend_from_slice(body);
        pos += len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_round_trip_small() {
        let mut buf = Vec::new();
        frame(&[1, 2, 3], &mut buf);
        assert_eq!(buf, vec![0, 3, 1, 2, 3, 0, 0]);
        assert_eq!(deframe(&buf), Ok((vec![1, 2, 3], 7)));
    }

    #[test]
    fn framing_eof_marker() {
        let mut buf = Vec::new();
        frame(&[], &mut buf);
        assert_eq!(buf, vec![0, 0]);
        // An empty message is indistinguishable from a no-op; nothing to read.
        assert_eq!(deframe(&buf), Err(FrameError::Incomplete));
        assert_eq!(deframe(&[0, 1, 9, 0, 0]), Ok((vec![9], 5)));
    }

    #[test]
    fn framing_chunk_boundary() {
        // Exactly one full chunk, one byte over, and two full chunks.
        for size in [MAX_CHUNK - 1, MAX_CHUNK, MAX_CHUNK + 1, 2 * MAX_CHUNK] {
            let msg: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut buf = Vec::new();
            frame(&msg, &mut buf);
            let chunks = size.div_ceil(MAX_CHUNK);
            assert_eq!(buf.len(), size + 2 * chunks + 2, "size {size}");
            assert_eq!(deframe(&buf), Ok((msg, buf.len())), "size {size}");
        }
    }

    #[test]
    fn framing_message_at_odd_offset() {
        let mut buf = vec![0xAA, 0xBB, 0xCC];
        let start = buf.len();
        frame(&[7, 8], &mut buf);
        frame(&[9], &mut buf);
        let (first, used) = deframe(&buf[start..]).unwrap();
        assert_eq!(first, vec![7, 8]);
        let (second, used2) = deframe(&buf[start + used..]).unwrap();
        assert_eq!(second, vec![9]);
        assert_eq!(start + used + used2, buf.len());
    }

    #[test]
    fn framing_multi_chunk_message_split_by_sender() {
        let stream = [0, 2, 1, 2, 0, 1, 3, 0, 0];
        assert_eq!(deframe(&stream), Ok((vec![1, 2, 3], 9)));
    }

    #[test]
    fn framing_noop_chunks_are_skipped() {
        let stream = [0, 0, 0, 0, 0, 1, 5, 0, 0];
        assert_eq!(deframe(&stream), Ok((vec![5], 9)));
    }

    #[test]
    fn framing_truncated_is_incomplete_at_every_cut() {
        let mut buf = Vec::new();
        frame(&[1, 2, 3, 4], &mut buf);
        for cut in 0..buf.len() {
            assert_eq!(
                deframe(&buf[..cut]),
                Err(FrameError::Incomplete),
                "cut {cut}"
            );
        }
    }
}
