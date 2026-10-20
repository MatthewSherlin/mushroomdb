//! Tailing `wal.bin`: the next byte range to ship, in whole frames.
//!
//! A WAL frame is `[len: u32 LE][crc32: u32 LE][payload: len bytes]`
//! (`core_storage::wal::encode_record`). A segment always ends on a frame
//! boundary, so concatenating a generation's segments in offset order is a
//! valid WAL.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// Frame header: `len` then `crc32`, both u32 little-endian.
pub const FRAME_HEADER_LEN: usize = 8;

/// Most bytes one call reads, unless a single frame is larger. A WAL that grew
/// by more than this since the last tick ships over several segments.
pub const MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

/// The prefix of `bytes` made of whole, checksum-valid frames: its length and
/// how many frames it holds.
///
/// Stops at the first frame that is incomplete or whose checksum does not
/// match — the same rule the engine's own decoder applies.
pub fn complete_frames(bytes: &[u8]) -> (usize, u64) {
    let mut pos = 0usize;
    let mut frames = 0u64;
    while let Some(len) = frame_len_at(bytes, pos) {
        let start = pos + FRAME_HEADER_LEN;
        let Some(end) = start.checked_add(len).filter(|&e| e <= bytes.len()) else {
            break;
        };
        let crc = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        if crc32fast::hash(&bytes[start..end]) != crc {
            break;
        }
        pos = end;
        frames += 1;
    }
    (pos, frames)
}

fn frame_len_at(bytes: &[u8], pos: usize) -> Option<usize> {
    let header = bytes.get(pos..pos + FRAME_HEADER_LEN)?;
    Some(u32::from_le_bytes(header[..4].try_into().unwrap()) as usize)
}

/// The next segment of `wal_path` to ship: bytes from `from_byte` up to at most
/// `durable_len`, cut back to the last complete frame.
///
/// `durable_len` is what the engine reports as fsynced
/// (`SharedDb::durable_wal_position`) — never the file's length, which counts
/// bytes still in the page cache. Returns `(start, end, bytes)`, or `None`
/// when there is no whole frame to ship (nothing new, the file is missing or
/// shorter than `from_byte`, or only part of a frame is there).
///
/// # Errors
///
/// An I/O error reading the file, or `InvalidData` when the bytes at
/// `from_byte` are long enough to hold a frame but are not one — the cursor is
/// not on a frame boundary, or the WAL is corrupt — which a retry cannot fix
/// and must not be shipped.
pub fn read_new_segment(
    wal_path: &Path,
    from_byte: u64,
    durable_len: u64,
) -> io::Result<Option<(u64, u64, Vec<u8>)>> {
    if durable_len <= from_byte {
        return Ok(None);
    }
    let mut file = match std::fs::File::open(wal_path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let available = durable_len.min(file.metadata()?.len());
    if available <= from_byte {
        return Ok(None);
    }
    let want = (available - from_byte).min(MAX_SEGMENT_BYTES);
    file.seek(SeekFrom::Start(from_byte))?;
    let mut bytes = read_up_to(&mut file, want)?;
    // A single frame larger than the cap: read exactly that frame.
    if let Some(len) = frame_len_at(&bytes, 0) {
        let frame = (FRAME_HEADER_LEN + len) as u64;
        if frame > bytes.len() as u64 && from_byte + frame <= available {
            let rest = read_up_to(&mut file, frame - bytes.len() as u64)?;
            bytes.extend_from_slice(&rest);
        }
    }
    let (whole, _) = complete_frames(&bytes);
    if whole == 0 {
        let first_frame_is_there = frame_len_at(&bytes, 0)
            .is_some_and(|len| (FRAME_HEADER_LEN + len) as u64 <= bytes.len() as u64);
        if first_frame_is_there {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("wal.bin at byte {from_byte} is not a valid frame; not shipping it"),
            ));
        }
        return Ok(None);
    }
    bytes.truncate(whole);
    Ok(Some((from_byte, from_byte + whole as u64, bytes)))
}

fn read_up_to(file: &mut std::fs::File, n: u64) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(n as usize);
    file.take(n).read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend((payload.len() as u32).to_le_bytes());
        f.extend(crc32fast::hash(payload).to_le_bytes());
        f.extend(payload);
        f
    }

    #[test]
    fn complete_frames_counts_whole_valid_frames() {
        let mut b = frame(b"one");
        b.extend(frame(b"two"));
        let two = b.len();
        b.extend(&frame(b"three")[..5]);
        assert_eq!(complete_frames(&b), (two, 2));
        assert_eq!(complete_frames(&[]), (0, 0));
    }

    #[test]
    fn a_bad_checksum_stops_the_scan() {
        let mut b = frame(b"one");
        let one = b.len();
        let mut bad = frame(b"two");
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        b.extend(bad);
        assert_eq!(complete_frames(&b), (one, 1));
    }

    #[test]
    fn nothing_past_the_durable_length_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wal.bin");
        let mut b = frame(b"one");
        let one = b.len() as u64;
        b.extend(frame(b"two"));
        std::fs::write(&p, &b).unwrap();
        let (s, e, bytes) = read_new_segment(&p, 0, one).unwrap().unwrap();
        assert_eq!((s, e), (0, one));
        assert_eq!(bytes, frame(b"one"));
        assert!(read_new_segment(&p, one, one).unwrap().is_none());
    }

    #[test]
    fn a_whole_frame_that_fails_its_checksum_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wal.bin");
        let mut b = frame(b"0123456789abcdef");
        let last = b.len() - 1;
        b[last] ^= 0xff;
        std::fs::write(&p, &b).unwrap();
        let err = read_new_segment(&p, 0, b.len() as u64).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_missing_file_has_nothing_to_ship() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_new_segment(&dir.path().join("wal.bin"), 0, 100)
            .unwrap()
            .is_none());
    }
}
