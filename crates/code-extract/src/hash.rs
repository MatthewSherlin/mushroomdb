//! Content hashing and line counting.
//!
//! The hash is the first 16 bytes of the BLAKE3 digest of the raw file bytes,
//! rendered as 32 lowercase hex characters. Truncation keeps node properties
//! small; 128 bits is far more than enough to detect a changed working-tree
//! file, which is the only thing the hash is used for.

/// Hash `bytes` to 32 lowercase hex characters.
pub(crate) fn hex32(bytes: &[u8]) -> String {
    let digest = blake3::hash(bytes);
    let mut out = String::with_capacity(32);
    for byte in &digest.as_bytes()[..16] {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Count lines the way an editor does: one per newline, plus a final line when
/// the content does not end in a newline. An empty file has zero lines.
pub(crate) fn count_lines(bytes: &[u8]) -> u32 {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|b| **b == b'\n').count();
    let trailing = usize::from(bytes.last() != Some(&b'\n'));
    u32::try_from(newlines + trailing).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex32_is_32_lowercase_hex_chars() {
        let h = hex32(b"hello");
        assert_eq!(h.len(), 32);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn line_counts_match_editor_conventions() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a"), 1);
        assert_eq!(count_lines(b"a\n"), 1);
        assert_eq!(count_lines(b"a\nb"), 2);
        assert_eq!(count_lines(b"a\nb\n"), 2);
    }
}
