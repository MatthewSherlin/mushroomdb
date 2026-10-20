//! PackStream v1: the binary serialization every Bolt message is built from.
//!
//! `pack` always writes the smallest encoding of a value. `unpack` accepts any
//! valid encoding (including non-minimal ones) and never panics on malformed
//! input: truncation, reserved markers, oversized lengths and runaway nesting
//! all return a `PackError`.
//!
//! There is deliberately no conversion from engine values here. A `PackValue`
//! is the wire shape only; mapping engine values (and the temporal and binary
//! types) onto it belongs to the layer that owns the result rows.

use std::fmt;

/// Bolt structure tag for a node.
pub const TAG_NODE: u8 = 0x4E;
/// Bolt structure tag for a relationship.
pub const TAG_RELATIONSHIP: u8 = 0x52;
/// Bolt structure tag for an unbound relationship (inside a path).
pub const TAG_UNBOUND_RELATIONSHIP: u8 = 0x72;
/// Bolt structure tag for a path.
pub const TAG_PATH: u8 = 0x50;

/// Deepest list/map/struct nesting `unpack` accepts.
pub const MAX_DEPTH: usize = 128;

/// A PackStream value as it appears on the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum PackValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Bytes(Vec<u8>),
    String(String),
    List(Vec<PackValue>),
    /// Entries keep their wire order so a decoded map re-encodes identically.
    Map(Vec<(String, PackValue)>),
    /// A tagged structure. Graph entities and, later, the temporal types are
    /// all structures; an unrecognised tag decodes here rather than failing,
    /// so new struct types need no change to the decoder.
    Struct(PackStruct),
}

/// A PackStream structure: a one-byte tag and its fields.
#[derive(Debug, Clone, PartialEq)]
pub struct PackStruct {
    pub tag: u8,
    pub fields: Vec<PackValue>,
}

impl PackStruct {
    pub fn new(tag: u8, fields: Vec<PackValue>) -> Self {
        Self { tag, fields }
    }

    /// Node: `(id, labels, properties)`.
    pub fn node(id: i64, labels: Vec<String>, properties: Vec<(String, PackValue)>) -> Self {
        Self::new(
            TAG_NODE,
            vec![
                PackValue::Int(id),
                PackValue::List(labels.into_iter().map(PackValue::String).collect()),
                PackValue::Map(properties),
            ],
        )
    }

    /// Relationship: `(id, start, end, type, properties)`.
    pub fn relationship(
        id: i64,
        start: i64,
        end: i64,
        rel_type: String,
        properties: Vec<(String, PackValue)>,
    ) -> Self {
        Self::new(
            TAG_RELATIONSHIP,
            vec![
                PackValue::Int(id),
                PackValue::Int(start),
                PackValue::Int(end),
                PackValue::String(rel_type),
                PackValue::Map(properties),
            ],
        )
    }

    /// Path: `(nodes, unbound relationships, sequence)`.
    pub fn path(nodes: Vec<PackValue>, rels: Vec<PackValue>, sequence: Vec<i64>) -> Self {
        Self::new(
            TAG_PATH,
            vec![
                PackValue::List(nodes),
                PackValue::List(rels),
                PackValue::List(sequence.into_iter().map(PackValue::Int).collect()),
            ],
        )
    }
}

/// Why `unpack` refused a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackError {
    /// The buffer ended before the value did.
    Truncated,
    /// A marker byte that PackStream v1 reserves.
    ReservedMarker(u8),
    /// A string was not valid UTF-8.
    InvalidUtf8,
    /// A map key was not a string.
    NonStringKey,
    /// Nesting went deeper than `MAX_DEPTH`.
    TooDeep,
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackError::Truncated => write!(f, "packstream: buffer ended inside a value"),
            PackError::ReservedMarker(m) => write!(f, "packstream: reserved marker 0x{m:02X}"),
            PackError::InvalidUtf8 => write!(f, "packstream: string is not valid UTF-8"),
            PackError::NonStringKey => write!(f, "packstream: map key is not a string"),
            PackError::TooDeep => write!(f, "packstream: nesting deeper than {MAX_DEPTH}"),
        }
    }
}

impl std::error::Error for PackError {}

// ---------------------------------------------------------------- encoding

/// Append the encoding of `val` to `buf`.
pub fn pack(val: &PackValue, buf: &mut Vec<u8>) {
    match val {
        PackValue::Null => buf.push(0xC0),
        PackValue::Bool(false) => buf.push(0xC2),
        PackValue::Bool(true) => buf.push(0xC3),
        PackValue::Int(i) => pack_int(*i, buf),
        PackValue::Float(x) => {
            buf.push(0xC1);
            buf.extend_from_slice(&x.to_be_bytes());
        }
        PackValue::Bytes(b) => {
            pack_size(b.len(), [0xCC, 0xCD, 0xCE], None, buf);
            buf.extend_from_slice(b);
        }
        PackValue::String(s) => {
            pack_size(s.len(), [0xD0, 0xD1, 0xD2], Some(0x80), buf);
            buf.extend_from_slice(s.as_bytes());
        }
        PackValue::List(items) => {
            pack_size(items.len(), [0xD4, 0xD5, 0xD6], Some(0x90), buf);
            for item in items {
                pack(item, buf);
            }
        }
        PackValue::Map(entries) => {
            pack_size(entries.len(), [0xD8, 0xD9, 0xDA], Some(0xA0), buf);
            for (key, value) in entries {
                pack(&PackValue::String(key.clone()), buf);
                pack(value, buf);
            }
        }
        PackValue::Struct(s) => {
            let n = s.fields.len();
            if n < 16 {
                buf.push(0xB0 | n as u8);
            } else if n <= 0xFF {
                buf.extend_from_slice(&[0xDC, n as u8]);
            } else {
                // A struct size field is at most 16 bits wide; the cast keeps
                // the encoder total, and no Bolt structure has this many fields.
                buf.push(0xDD);
                buf.extend_from_slice(&(n as u16).to_be_bytes());
            }
            buf.push(s.tag);
            for field in &s.fields {
                pack(field, buf);
            }
        }
    }
}

fn pack_int(i: i64, buf: &mut Vec<u8>) {
    if (-16..=127).contains(&i) {
        buf.push(i as u8);
    } else if let Ok(v) = i8::try_from(i) {
        buf.extend_from_slice(&[0xC8, v as u8]);
    } else if let Ok(v) = i16::try_from(i) {
        buf.push(0xC9);
        buf.extend_from_slice(&v.to_be_bytes());
    } else if let Ok(v) = i32::try_from(i) {
        buf.push(0xCA);
        buf.extend_from_slice(&v.to_be_bytes());
    } else {
        buf.push(0xCB);
        buf.extend_from_slice(&i.to_be_bytes());
    }
}

/// Write a size header. `markers` are the 8/16/32-bit-length markers; `tiny`
/// is the base of the 0..=15 inline form, absent for bytes (which has none).
fn pack_size(len: usize, markers: [u8; 3], tiny: Option<u8>, buf: &mut Vec<u8>) {
    if let (Some(base), true) = (tiny, len < 16) {
        buf.push(base | len as u8);
    } else if len <= 0xFF {
        buf.extend_from_slice(&[markers[0], len as u8]);
    } else if len <= 0xFFFF {
        buf.push(markers[1]);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(markers[2]);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    }
}

// ---------------------------------------------------------------- decoding

/// Decode one value from the front of `buf`, returning it and the number of
/// bytes it occupied.
pub fn unpack(buf: &[u8]) -> Result<(PackValue, usize), PackError> {
    let mut cursor = Cursor { buf, pos: 0 };
    let value = cursor.value(0)?;
    Ok((value, cursor.pos))
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PackError> {
        let end = self.pos.checked_add(n).ok_or(PackError::Truncated)?;
        let slice = self.buf.get(self.pos..end).ok_or(PackError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, PackError> {
        Ok(self.take(1)?[0])
    }

    fn uint(&mut self, width: usize) -> Result<usize, PackError> {
        let bytes = self.take(width)?;
        let n = bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
        usize::try_from(n).map_err(|_| PackError::Truncated)
    }

    /// Read the size that follows a 8/16/32-bit-length marker.
    fn sized(&mut self, marker: u8, markers: [u8; 3]) -> Result<usize, PackError> {
        let width = if marker == markers[0] {
            1
        } else if marker == markers[1] {
            2
        } else {
            4
        };
        self.uint(width)
    }

    fn string(&mut self, len: usize) -> Result<String, PackError> {
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| PackError::InvalidUtf8)
    }

    // Each element needs at least one byte, so a declared count larger than
    // the bytes left cannot be satisfied; refusing it up front keeps a hostile
    // header from reserving gigabytes.
    fn check_count(&self, count: usize) -> Result<(), PackError> {
        if count > self.buf.len() - self.pos {
            return Err(PackError::Truncated);
        }
        Ok(())
    }

    fn list(&mut self, count: usize, depth: usize) -> Result<PackValue, PackError> {
        self.check_count(count)?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(self.value(depth + 1)?);
        }
        Ok(PackValue::List(items))
    }

    fn map(&mut self, count: usize, depth: usize) -> Result<PackValue, PackError> {
        self.check_count(count)?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let key = match self.value(depth + 1)? {
                PackValue::String(s) => s,
                _ => return Err(PackError::NonStringKey),
            };
            entries.push((key, self.value(depth + 1)?));
        }
        Ok(PackValue::Map(entries))
    }

    fn structure(&mut self, count: usize, depth: usize) -> Result<PackValue, PackError> {
        let tag = self.byte()?;
        self.check_count(count)?;
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            fields.push(self.value(depth + 1)?);
        }
        Ok(PackValue::Struct(PackStruct { tag, fields }))
    }

    fn value(&mut self, depth: usize) -> Result<PackValue, PackError> {
        if depth > MAX_DEPTH {
            return Err(PackError::TooDeep);
        }
        let marker = self.byte()?;
        match marker {
            0x00..=0x7F => Ok(PackValue::Int(i64::from(marker))),
            0xF0..=0xFF => Ok(PackValue::Int(i64::from(marker as i8))),
            0x80..=0x8F => Ok(PackValue::String(self.string(usize::from(marker & 0x0F))?)),
            0x90..=0x9F => self.list(usize::from(marker & 0x0F), depth),
            0xA0..=0xAF => self.map(usize::from(marker & 0x0F), depth),
            0xB0..=0xBF => self.structure(usize::from(marker & 0x0F), depth),
            0xC0 => Ok(PackValue::Null),
            0xC1 => {
                let bytes: [u8; 8] = self.take(8)?.try_into().map_err(|_| PackError::Truncated)?;
                Ok(PackValue::Float(f64::from_be_bytes(bytes)))
            }
            0xC2 => Ok(PackValue::Bool(false)),
            0xC3 => Ok(PackValue::Bool(true)),
            0xC8 => Ok(PackValue::Int(i64::from(self.byte()? as i8))),
            0xC9 => {
                let b = self.take(2)?;
                Ok(PackValue::Int(i64::from(i16::from_be_bytes([b[0], b[1]]))))
            }
            0xCA => {
                let b = self.take(4)?;
                Ok(PackValue::Int(i64::from(i32::from_be_bytes([
                    b[0], b[1], b[2], b[3],
                ]))))
            }
            0xCB => {
                let bytes: [u8; 8] = self.take(8)?.try_into().map_err(|_| PackError::Truncated)?;
                Ok(PackValue::Int(i64::from_be_bytes(bytes)))
            }
            0xCC..=0xCE => {
                let len = self.sized(marker, [0xCC, 0xCD, 0xCE])?;
                Ok(PackValue::Bytes(self.take(len)?.to_vec()))
            }
            0xD0..=0xD2 => {
                let len = self.sized(marker, [0xD0, 0xD1, 0xD2])?;
                Ok(PackValue::String(self.string(len)?))
            }
            0xD4..=0xD6 => {
                let count = self.sized(marker, [0xD4, 0xD5, 0xD6])?;
                self.list(count, depth)
            }
            0xD8..=0xDA => {
                let count = self.sized(marker, [0xD8, 0xD9, 0xDA])?;
                self.map(count, depth)
            }
            0xDC => {
                let count = self.uint(1)?;
                self.structure(count, depth)
            }
            0xDD => {
                let count = self.uint(2)?;
                self.structure(count, depth)
            }
            other => Err(PackError::ReservedMarker(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(v: &PackValue) -> Vec<u8> {
        let mut buf = Vec::new();
        pack(v, &mut buf);
        buf
    }

    fn round_trip(v: PackValue) -> Vec<u8> {
        let bytes = encode(&v);
        let (back, used) = unpack(&bytes).expect("decodes");
        assert_eq!(back, v);
        assert_eq!(used, bytes.len());
        bytes
    }

    #[test]
    fn round_trip_int() {
        // (value, expected encoded length): tiny, int8, int16, int32, int64.
        let cases: [(i64, usize); 13] = [
            (0, 1),
            (127, 1),
            (-16, 1),
            (-17, 2),
            (-128, 2),
            (128, 3),
            (-129, 3),
            (32767, 3),
            (32768, 5),
            (-32769, 5),
            (2_147_483_648, 9),
            (i64::MAX, 9),
            (i64::MIN, 9),
        ];
        for (n, len) in cases {
            assert_eq!(round_trip(PackValue::Int(n)).len(), len, "{n}");
        }
        assert_eq!(encode(&PackValue::Int(1)), [0x01]);
        assert_eq!(encode(&PackValue::Int(-1)), [0xFF]);
        assert_eq!(encode(&PackValue::Int(-17)), [0xC8, 0xEF]);
        assert_eq!(encode(&PackValue::Int(1234)), [0xC9, 0x04, 0xD2]);
    }

    #[test]
    fn round_trip_scalars() {
        round_trip(PackValue::Null);
        round_trip(PackValue::Bool(true));
        round_trip(PackValue::Bool(false));
        assert_eq!(encode(&PackValue::Null), [0xC0]);
        assert_eq!(encode(&PackValue::Bool(false)), [0xC2]);
        assert_eq!(encode(&PackValue::Bool(true)), [0xC3]);
        round_trip(PackValue::Bytes(vec![]));
        round_trip(PackValue::Bytes(vec![1, 2, 3]));
        assert_eq!(encode(&PackValue::Bytes(vec![9])), [0xCC, 0x01, 0x09]);
        round_trip(PackValue::Bytes(vec![7; 300]));
    }

    #[test]
    fn round_trip_float() {
        for x in [0.0, -0.0, 1.5, f64::MIN, f64::MAX, f64::INFINITY] {
            round_trip(PackValue::Float(x));
        }
        assert_eq!(
            encode(&PackValue::Float(1.1)),
            [0xC1, 0x3F, 0xF1, 0x99, 0x99, 0x99, 0x99, 0x99, 0x9A]
        );
        // NaN is never equal to itself, so compare the bits.
        let bytes = encode(&PackValue::Float(f64::NAN));
        match unpack(&bytes).unwrap().0 {
            PackValue::Float(x) => assert!(x.is_nan()),
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_str() {
        round_trip(PackValue::String(String::new()));
        assert_eq!(encode(&PackValue::String("A".into())), [0x81, 0x41]);
        let long = "é".repeat(10);
        assert_eq!(round_trip(PackValue::String(long))[0], 0xD0);
        assert_eq!(round_trip(PackValue::String("x".repeat(15)))[0], 0x8F);
        assert_eq!(round_trip(PackValue::String("x".repeat(16)))[0], 0xD0);
        assert_eq!(round_trip(PackValue::String("x".repeat(256)))[0], 0xD1);
        assert_eq!(round_trip(PackValue::String("x".repeat(70_000)))[0], 0xD2);
    }

    #[test]
    fn round_trip_list() {
        round_trip(PackValue::List(vec![]));
        assert_eq!(
            encode(&PackValue::List(vec![PackValue::Int(1), PackValue::Int(2)])),
            [0x92, 0x01, 0x02]
        );
        let nested = PackValue::List(vec![
            PackValue::List(vec![PackValue::Null, PackValue::Bool(true)]),
            PackValue::String("a".into()),
        ]);
        round_trip(nested);
        let wide = PackValue::List((0..20).map(PackValue::Int).collect());
        assert_eq!(round_trip(wide)[0], 0xD4);
    }

    #[test]
    fn round_trip_map() {
        round_trip(PackValue::Map(vec![]));
        let m = PackValue::Map(vec![
            ("b".into(), PackValue::Int(1)),
            ("a".into(), PackValue::List(vec![PackValue::Float(2.5)])),
        ]);
        // Wire order is preserved, not sorted.
        assert_eq!(encode(&m)[..3], [0xA2, 0x81, 0x62]);
        round_trip(m);
        let wide = PackValue::Map(
            (0..20)
                .map(|i| (format!("k{i}"), PackValue::Int(i)))
                .collect(),
        );
        assert_eq!(round_trip(wide)[0], 0xD8);
    }

    #[test]
    fn round_trip_node_struct() {
        let node = PackStruct::node(
            42,
            vec!["Person".into(), "Admin".into()],
            vec![("name".into(), PackValue::String("Ada".into()))],
        );
        assert_eq!(node.tag, 0x4E);
        let bytes = round_trip(PackValue::Struct(node));
        assert_eq!(bytes[..2], [0xB3, 0x4E]);
    }

    #[test]
    fn round_trip_relationship_struct() {
        let rel = PackStruct::relationship(
            7,
            1,
            2,
            "KNOWS".into(),
            vec![("since".into(), PackValue::Int(2020))],
        );
        assert_eq!(rel.tag, 0x52);
        let bytes = round_trip(PackValue::Struct(rel));
        assert_eq!(bytes[..2], [0xB5, 0x52]);
    }

    #[test]
    fn round_trip_path_struct() {
        let node = PackValue::Struct(PackStruct::node(1, vec![], vec![]));
        let rel = PackValue::Struct(PackStruct::new(
            TAG_UNBOUND_RELATIONSHIP,
            vec![
                PackValue::Int(5),
                PackValue::String("R".into()),
                PackValue::Map(vec![]),
            ],
        ));
        let path = PackStruct::path(vec![node.clone(), node], vec![rel], vec![1, 1]);
        assert_eq!(path.tag, 0x50);
        round_trip(PackValue::Struct(path));
    }

    #[test]
    fn unknown_struct_tag_and_wide_struct_round_trip() {
        round_trip(PackValue::Struct(PackStruct::new(0x7A, vec![])));
        let wide = PackStruct::new(0x01, (0..20).map(PackValue::Int).collect());
        assert_eq!(round_trip(PackValue::Struct(wide))[..2], [0xDC, 20]);
    }

    #[test]
    fn unpack_reports_bytes_consumed_and_ignores_the_rest() {
        let (v, used) = unpack(&[0x01, 0x02]).unwrap();
        assert_eq!((v, used), (PackValue::Int(1), 1));
    }

    #[test]
    fn unpack_accepts_non_minimal_ints() {
        let (v, used) = unpack(&[0xCB, 0, 0, 0, 0, 0, 0, 0, 5]).unwrap();
        assert_eq!((v, used), (PackValue::Int(5), 9));
    }

    #[test]
    fn unpack_rejects_malformed_input() {
        assert_eq!(unpack(&[]), Err(PackError::Truncated));
        assert_eq!(unpack(&[0xC9, 0x01]), Err(PackError::Truncated));
        assert_eq!(unpack(&[0x85, b'a']), Err(PackError::Truncated));
        assert_eq!(unpack(&[0xC4]), Err(PackError::ReservedMarker(0xC4)));
        assert_eq!(unpack(&[0xCF]), Err(PackError::ReservedMarker(0xCF)));
        assert_eq!(unpack(&[0xDF]), Err(PackError::ReservedMarker(0xDF)));
        assert_eq!(unpack(&[0x81, 0xFF]), Err(PackError::InvalidUtf8));
        assert_eq!(unpack(&[0xA1, 0x01, 0x01]), Err(PackError::NonStringKey));
        // A header claiming four billion elements with no bytes behind it.
        assert_eq!(
            unpack(&[0xD6, 0xFF, 0xFF, 0xFF, 0xFF]),
            Err(PackError::Truncated)
        );
        assert_eq!(
            unpack(&[0xCE, 0xFF, 0xFF, 0xFF, 0xFF]),
            Err(PackError::Truncated)
        );
    }

    #[test]
    fn unpack_rejects_runaway_nesting() {
        let deep = vec![0x91; MAX_DEPTH + 10];
        assert_eq!(unpack(&deep), Err(PackError::TooDeep));
        // Exactly at the limit decodes.
        let mut ok = vec![0x91; MAX_DEPTH];
        ok.push(0xC0);
        assert!(unpack(&ok).is_ok());
    }

    /// Small deterministic generator, so the fuzz run is reproducible and the
    /// crate needs no random-number dependency.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Random bytes are mostly valid PackStream (every byte below 0x80 is a
    /// whole integer), so the contract is not "always Err" but: never panic,
    /// and whatever decodes re-encodes to bytes that decode to the same value.
    #[test]
    fn fuzz_no_panic() {
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let (mut decoded, mut refused) = (0, 0);
        for _ in 0..20_000 {
            let len = (rng.next() % 64) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            match unpack(&bytes) {
                Ok((value, used)) => {
                    decoded += 1;
                    assert!(used <= bytes.len());
                    let first = encode(&value);
                    let (again, used_again) = unpack(&first).expect("own output decodes");
                    assert_eq!(used_again, first.len());
                    assert_eq!(encode(&again), first);
                }
                Err(_) => refused += 1,
            }
        }
        assert!(decoded > 0 && refused > 0, "{decoded} ok, {refused} err");
    }

    #[test]
    fn fuzz_every_truncation_of_a_valid_message_is_an_error() {
        let value = PackValue::Struct(PackStruct::node(
            1_000_000,
            vec!["L".into()],
            vec![
                ("k".into(), PackValue::List(vec![PackValue::Float(1.5)])),
                ("s".into(), PackValue::String("x".repeat(40))),
            ],
        ));
        let bytes = encode(&value);
        for cut in 0..bytes.len() {
            assert!(unpack(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        assert!(unpack(&bytes).is_ok());
    }
}
