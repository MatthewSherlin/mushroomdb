use crate::cow::{CowMap, CowVec, HashIndex};
use crate::pack::{push_str, push_u32, read_exact, read_str, read_u32, read_u32s};
use crate::types::Result as StoreResult;
use crate::types::{GraphError, Value};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

// Every structure here is copy-on-write in pieces (`crate::cow`): the write
// path shares the column store with the reader's fold, and the first commit
// after a fold copies only the chunks it writes, not the store (ledger row
// 126).

/// Presence bitmap: bit i is set iff node i has a value. Unset bits are nulls.
#[derive(Debug, Default, Clone)]
struct Bitmap {
    bits: CowVec<u64>,
}

impl Bitmap {
    fn contains(&self, i: u32) -> bool {
        let i = i as usize;
        let word = i / 64;
        let bit = i % 64;
        self.bits.get(word).is_some_and(|w| w & (1u64 << bit) != 0)
    }

    /// Set bit `i`. Returns true if it was previously unset.
    fn set(&mut self, i: u32) -> bool {
        let i = i as usize;
        let word = i / 64;
        let bit = i % 64;
        if word >= self.bits.len() {
            self.bits.grow_to(word + 1, 0);
        }
        let mask = 1u64 << bit;
        let w = self.bits[word];
        if w & mask != 0 {
            return false;
        }
        self.bits.set(word, w | mask);
        true
    }

    /// Clear bit `i`. Returns true if it was previously set.
    fn clear(&mut self, i: u32) -> bool {
        let i = i as usize;
        let word = i / 64;
        let bit = i % 64;
        let Some(&w) = self.bits.get(word) else {
            return false;
        };
        let mask = 1u64 << bit;
        if w & mask == 0 {
            return false;
        }
        self.bits.set(word, w & !mask);
        true
    }

    fn for_each(&self, mut f: impl FnMut(u32)) {
        for (wi, &word) in self.bits.iter().enumerate() {
            let mut w = word;
            while w != 0 {
                let b = w.trailing_zeros();
                f(wi as u32 * 64 + b);
                w &= w - 1;
            }
        }
    }

    fn live_count(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }

    fn pack(&self, out: &mut Vec<u8>) {
        push_u32(out, self.bits.len() as u32);
        for w in self.bits.iter() {
            out.extend_from_slice(&w.to_le_bytes());
        }
    }

    fn unpack(src: &[u8], pos: &mut usize) -> StoreResult<Self> {
        let n = read_u32(src, pos)? as usize;
        let bytes = read_exact(src, pos, n.saturating_mul(8))?;
        let mut bits = Vec::with_capacity(n);
        for chunk in bytes.chunks_exact(8) {
            bits.push(u64::from_le_bytes(chunk.try_into().unwrap())); // infallible: chunks_exact(8) yields 8-byte slices
        }
        Ok(Self {
            bits: CowVec::from_vec(bits),
        })
    }
}

/// Append-only intern of `Value::Str`. Homogeneous string columns store ids here.
///
/// Strings are found through a [`HashIndex`], which holds hashes and not the
/// strings, so a clone copies no string and a new string copies one shard.
#[derive(Debug, Default, Clone)]
struct StrIntern {
    index: HashIndex,
    values: CowVec<Value>,
}

impl StrIntern {
    fn find(&self, s: &str, h: u64) -> Option<u32> {
        self.index
            .get_hashed(s, h, |id| match self.values.get(id as usize) {
                Some(Value::Str(x)) => Some(x.as_str()),
                _ => None,
            })
    }

    fn intern(&mut self, s: String) -> u32 {
        let h = self.index.hash(&s);
        if let Some(id) = self.find(&s, h) {
            return id;
        }
        let id = self.values.len() as u32;
        self.index.insert_hashed(&s, h, id);
        self.values.push(Value::Str(s));
        id
    }

    fn get(&self, id: u32) -> &Value {
        &self.values[id as usize]
    }

    fn pack(&self, out: &mut Vec<u8>) {
        push_u32(out, self.values.len() as u32);
        for v in self.values.iter() {
            let Value::Str(s) = v else {
                unreachable!("StrIntern values are always Value::Str");
            };
            push_str(out, s);
        }
    }

    fn unpack(src: &[u8], pos: &mut usize) -> StoreResult<Self> {
        let n = read_u32(src, pos)? as usize;
        let mut intern = Self::default();
        for i in 0..n {
            let s = read_str(src, pos)?;
            // A string an image lists twice keeps its first id, as `intern`
            // would have given it.
            let h = intern.index.hash(&s);
            if intern.find(&s, h).is_none() {
                intern.index.insert_hashed(&s, h, i as u32);
            }
            intern.values.push(Value::Str(s));
        }
        Ok(intern)
    }
}

/// The scalar types a fixed-width column holds, and their pack tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fixed {
    Int,
    Float,
    Bool,
    /// `Value::Date` as days since 1970-01-01.
    Date,
    /// `Value::LocalDateTime` as microseconds since the epoch, no offset.
    Ldt,
    /// `Value::DateTime` as `(micros_utc, offset_seconds)`.
    Dt,
    /// `Value::Duration` as `(months, days, nanos)`.
    Dur,
}

impl Fixed {
    fn of(value: &Value) -> Option<Self> {
        Some(match value {
            Value::Int(_) => Fixed::Int,
            Value::Float(_) => Fixed::Float,
            Value::Bool(_) => Fixed::Bool,
            Value::Date(_) => Fixed::Date,
            Value::LocalDateTime(_) => Fixed::Ldt,
            Value::DateTime { .. } => Fixed::Dt,
            Value::Duration { .. } => Fixed::Dur,
            _ => return None,
        })
    }

    /// What an unset slot holds: never read, since `present` is clear.
    fn filler(self) -> Value {
        match self {
            Fixed::Int => Value::Int(0),
            Fixed::Float => Value::Float(0.0),
            Fixed::Bool => Value::Bool(false),
            Fixed::Date => Value::Date(0),
            Fixed::Ldt => Value::LocalDateTime(0),
            Fixed::Dt => Value::DateTime {
                micros_utc: 0,
                offset_seconds: 0,
            },
            Fixed::Dur => Value::Duration {
                months: 0,
                days: 0,
                nanos: 0,
            },
        }
    }

    fn tag(self) -> u8 {
        match self {
            Fixed::Int => 0,
            Fixed::Float => 1,
            Fixed::Bool => 2,
            Fixed::Date => 5,
            Fixed::Ldt => 6,
            Fixed::Dt => 7,
            Fixed::Dur => 8,
        }
    }

    /// Bytes per packed slot.
    fn width(self) -> usize {
        match self {
            Fixed::Bool => 1,
            Fixed::Date => 4,
            Fixed::Int | Fixed::Float | Fixed::Ldt => 8,
            Fixed::Dt => 12,
            Fixed::Dur => 16,
        }
    }

    /// Append the raw little-endian form of `v`, a value of this type.
    fn push_raw(self, v: &Value, out: &mut Vec<u8>) {
        match (self, v) {
            (Fixed::Int, Value::Int(i)) | (Fixed::Ldt, Value::LocalDateTime(i)) => {
                out.extend_from_slice(&i.to_le_bytes())
            }
            (Fixed::Float, Value::Float(f)) => out.extend_from_slice(&f.to_le_bytes()),
            (Fixed::Bool, Value::Bool(b)) => out.push(u8::from(*b)),
            (Fixed::Date, Value::Date(d)) => out.extend_from_slice(&d.to_le_bytes()),
            (
                Fixed::Dt,
                Value::DateTime {
                    micros_utc,
                    offset_seconds,
                },
            ) => {
                out.extend_from_slice(&micros_utc.to_le_bytes());
                out.extend_from_slice(&offset_seconds.to_le_bytes());
            }
            (
                Fixed::Dur,
                Value::Duration {
                    months,
                    days,
                    nanos,
                },
            ) => {
                out.extend_from_slice(&months.to_le_bytes());
                out.extend_from_slice(&days.to_le_bytes());
                out.extend_from_slice(&nanos.to_le_bytes());
            }
            _ => unreachable!("a fixed column holds only its own type"),
        }
    }

    /// The value one packed slot of this type holds.
    fn read_raw(self, b: &[u8]) -> Value {
        match self {
            Fixed::Int => Value::Int(le_i64(b)),
            Fixed::Float => Value::Float(f64::from_le_bytes(b[..8].try_into().unwrap())),
            Fixed::Bool => Value::Bool(b[0] != 0),
            Fixed::Date => Value::Date(le_i32(b)),
            Fixed::Ldt => Value::LocalDateTime(le_i64(b)),
            Fixed::Dt => Value::DateTime {
                micros_utc: le_i64(&b[..8]),
                offset_seconds: le_i32(&b[8..]),
            },
            Fixed::Dur => Value::Duration {
                months: le_i32(&b[..4]),
                days: le_i32(&b[4..8]),
                nanos: le_i64(&b[8..]),
            },
        }
    }
}

#[derive(Debug, Clone)]
enum Column {
    /// A homogeneous fixed-width scalar column. Each slot holds its `Value`
    /// (the type's filler when unset), so `get` hands out a `&Value` with no
    /// conversion, and a write touches one chunk. The packed form is the raw
    /// scalars, derived from the values when the column is packed.
    Fixed {
        kind: Fixed,
        values: CowVec<Value>,
        present: Bitmap,
        live: usize,
    },
    Str {
        ids: CowVec<u32>,
        present: Bitmap,
        live: usize,
    },
    /// `Value::Binary`. Each slot holds a `Value::Binary` inline (an empty one
    /// for an unset slot), so `get` can hand out a `&Value` with no adapter.
    Binary {
        data: CowVec<Value>,
        present: Bitmap,
        live: usize,
    },
    /// Slow path: mixed-type fields and `Value::List`. Never demoted back to homogeneous.
    Mixed(CowMap<u32, Value>),
}

fn empty_binary() -> Value {
    Value::Binary(Vec::new())
}

/// Read a count-prefixed run of `width`-byte records. The byte length is
/// checked against the buffer before anything is allocated.
fn read_records<'a>(
    src: &'a [u8],
    pos: &mut usize,
    width: usize,
) -> StoreResult<std::slice::ChunksExact<'a, u8>> {
    let n = read_u32(src, pos)? as usize;
    let len = n.checked_mul(width).ok_or_else(|| GraphError::Corrupt {
        detail: "snapshot: packed length overflow".into(),
    })?;
    Ok(read_exact(src, pos, len)?.chunks_exact(width))
}

// Infallible below: `chunks_exact(width)` yields `width`-byte slices.
fn le_i32(b: &[u8]) -> i32 {
    i32::from_le_bytes(b[..4].try_into().unwrap())
}

fn le_i64(b: &[u8]) -> i64 {
    i64::from_le_bytes(b[..8].try_into().unwrap())
}

impl Column {
    fn from_first(node: u32, value: Value, intern: &mut StrIntern) -> Self {
        let mut col = match &value {
            Value::Str(_) => Column::Str {
                ids: CowVec::new(),
                present: Bitmap::default(),
                live: 0,
            },
            Value::Binary(_) => Column::Binary {
                data: CowVec::new(),
                present: Bitmap::default(),
                live: 0,
            },
            other => match Fixed::of(other) {
                Some(kind) => Column::Fixed {
                    kind,
                    values: CowVec::new(),
                    present: Bitmap::default(),
                    live: 0,
                },
                // List and Map spill to the Mixed path and never promote a
                // typed column. Null is never reached: every write path
                // refuses a null value before it gets here (see
                // `Value::contains_null`); Mixed is the safe home.
                None => Column::Mixed(CowMap::new()),
            },
        };
        col.set(node, value, intern);
        col
    }

    fn accepts(&self, value: &Value) -> bool {
        match self {
            Column::Fixed { kind, .. } => Fixed::of(value) == Some(*kind),
            Column::Str { .. } => matches!(value, Value::Str(_)),
            Column::Binary { .. } => matches!(value, Value::Binary(_)),
            Column::Mixed(_) => true,
        }
    }

    fn set(&mut self, node: u32, value: Value, intern: &mut StrIntern) {
        if !self.accepts(&value) {
            let mut map = self.take_map(intern);
            map.insert(node, value);
            *self = Column::Mixed(map);
            return;
        }
        let i = node as usize;
        match (self, value) {
            (
                Column::Fixed {
                    kind,
                    values,
                    present,
                    live,
                },
                v,
            ) => {
                if values.len() <= i {
                    values.grow_to(i + 1, kind.filler());
                }
                values.set(i, v);
                if present.set(node) {
                    *live += 1;
                }
            }
            (Column::Str { ids, present, live }, Value::Str(s)) => {
                let id = intern.intern(s);
                if ids.len() <= i {
                    ids.grow_to(i + 1, 0);
                }
                ids.set(i, id);
                if present.set(node) {
                    *live += 1;
                }
            }
            (
                Column::Binary {
                    data,
                    present,
                    live,
                },
                v @ Value::Binary(_),
            ) => {
                if data.len() <= i {
                    data.grow_to(i + 1, empty_binary());
                }
                data.set(i, v);
                if present.set(node) {
                    *live += 1;
                }
            }
            (Column::Mixed(map), v) => {
                map.insert(node, v);
            }
            _ => unreachable!("accepts() rejected a matching type"),
        }
    }

    fn get<'a>(&'a self, node: u32, intern: &'a StrIntern) -> Option<&'a Value> {
        match self {
            Column::Fixed {
                values, present, ..
            }
            | Column::Binary {
                data: values,
                present,
                ..
            } => {
                if present.contains(node) {
                    values.get(node as usize)
                } else {
                    None
                }
            }
            Column::Str { ids, present, .. } => {
                if present.contains(node) {
                    Some(intern.get(ids[node as usize]))
                } else {
                    None
                }
            }
            Column::Mixed(map) => map.get(&node),
        }
    }

    fn remove(&mut self, node: u32, intern: &StrIntern) -> Option<Value> {
        match self {
            Column::Fixed {
                values,
                present,
                live,
                ..
            } => {
                if !present.clear(node) {
                    return None;
                }
                *live -= 1;
                Some(values[node as usize].clone())
            }
            Column::Str { ids, present, live } => {
                if !present.clear(node) {
                    return None;
                }
                *live -= 1;
                Some(intern.get(ids[node as usize]).clone())
            }
            Column::Binary {
                data,
                present,
                live,
            } => {
                if !present.clear(node) {
                    return None;
                }
                *live -= 1;
                Some(data.replace(node as usize, empty_binary()))
            }
            Column::Mixed(map) => map.remove(&node),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Column::Mixed(map) => map.is_empty(),
            Column::Fixed { live, .. } | Column::Str { live, .. } | Column::Binary { live, .. } => {
                *live == 0
            }
        }
    }

    fn take_map(&mut self, intern: &StrIntern) -> CowMap<u32, Value> {
        match std::mem::replace(self, Column::Mixed(CowMap::new())) {
            Column::Mixed(map) => map,
            other => {
                let mut entries: Vec<(u32, Value)> = other.to_map(intern).into_iter().collect();
                entries.sort_unstable_by_key(|(node, _)| *node);
                CowMap::from_sorted(entries)
            }
        }
    }

    fn pack(&self, intern: &StrIntern, out: &mut Vec<u8>) -> StoreResult<()> {
        match self {
            // Tags 5-9 reach disk only in a V11 snapshot: the snapshot encoder
            // (`v8::encode::decode_all_columns`) refuses them for any other
            // version, and a store that writes one of these values is moved to
            // V11 at the WAL choke point before the value is appended.
            Column::Fixed {
                kind,
                values,
                present,
                ..
            } => {
                out.push(kind.tag());
                push_u32(out, values.len() as u32);
                out.reserve(values.len().saturating_mul(kind.width()));
                for v in values.iter() {
                    kind.push_raw(v, out);
                }
                present.pack(out);
            }
            Column::Str { ids, present, .. } => {
                out.push(3);
                push_u32(out, ids.len() as u32);
                out.reserve(ids.len().saturating_mul(4));
                for id in ids.iter() {
                    out.extend_from_slice(&id.to_le_bytes());
                }
                present.pack(out);
            }
            Column::Binary { data, present, .. } => {
                out.push(9);
                push_u32(out, data.len() as u32);
                for v in data.iter() {
                    let Value::Binary(bytes) = v else {
                        unreachable!("Binary column slots are always Value::Binary");
                    };
                    push_u32(out, bytes.len() as u32);
                    out.extend_from_slice(bytes);
                }
                present.pack(out);
            }
            Column::Mixed(map) => {
                if map.values().any(Value::contains_null) {
                    return Err(null_in_column());
                }
                out.push(4);
                let map: HashMap<u32, Value> = map.iter().map(|(k, v)| (*k, v.clone())).collect();
                let blob = bincode::serialize(&map).map_err(|e| GraphError::Corrupt {
                    detail: format!("mixed column failed to encode: {e}"),
                })?;
                push_u32(out, blob.len() as u32);
                out.extend_from_slice(&blob);
            }
        }
        let _ = intern;
        Ok(())
    }

    fn unpack(src: &[u8], pos: &mut usize) -> StoreResult<Self> {
        // Infallible: `read_exact(src, pos, 1)` returns a 1-byte slice; `first()` cannot return None.
        let tag = *read_exact(src, pos, 1)?.first().unwrap();
        let fixed = match tag {
            0 => Some(Fixed::Int),
            1 => Some(Fixed::Float),
            2 => Some(Fixed::Bool),
            5 => Some(Fixed::Date),
            6 => Some(Fixed::Ldt),
            7 => Some(Fixed::Dt),
            8 => Some(Fixed::Dur),
            _ => None,
        };
        if let Some(kind) = fixed {
            let values: CowVec<Value> = read_records(src, pos, kind.width())?
                .map(|b| kind.read_raw(b))
                .collect();
            let present = Bitmap::unpack(src, pos)?;
            let live = present.live_count();
            return Ok(Column::Fixed {
                kind,
                values,
                present,
                live,
            });
        }
        match tag {
            3 => {
                let ids = read_u32s(src, pos)?;
                let present = Bitmap::unpack(src, pos)?;
                let live = present.live_count();
                Ok(Column::Str {
                    ids: CowVec::from_vec(ids),
                    present,
                    live,
                })
            }
            4 => {
                let n = read_u32(src, pos)? as usize;
                let blob = read_exact(src, pos, n)?;
                let map: HashMap<u32, Value> =
                    bincode::deserialize(blob).map_err(|e| GraphError::Corrupt {
                        detail: format!("snapshot: mixed column: {e}"),
                    })?;
                let mut entries: Vec<(u32, Value)> = map.into_iter().collect();
                entries.sort_unstable_by_key(|(node, _)| *node);
                Ok(Column::Mixed(CowMap::from_sorted(entries)))
            }
            9 => {
                let n = read_u32(src, pos)? as usize;
                // Each slot costs at least 4 bytes, so a corrupt count cannot
                // reserve more than the buffer could hold.
                let mut data = Vec::with_capacity(n.min(src.len().saturating_sub(*pos) / 4));
                for _ in 0..n {
                    let len = read_u32(src, pos)? as usize;
                    data.push(Value::Binary(read_exact(src, pos, len)?.to_vec()));
                }
                let present = Bitmap::unpack(src, pos)?;
                let live = present.live_count();
                Ok(Column::Binary {
                    data: CowVec::from_vec(data),
                    present,
                    live,
                })
            }
            other => Err(GraphError::Corrupt {
                detail: format!("snapshot: unknown column tag {other}"),
            }),
        }
    }

    fn to_map(&self, intern: &StrIntern) -> HashMap<u32, Value> {
        match self {
            Column::Mixed(map) => map.iter().map(|(k, v)| (*k, v.clone())).collect(),
            Column::Fixed {
                values,
                present,
                live,
                ..
            }
            | Column::Binary {
                data: values,
                present,
                live,
            } => {
                let mut map = HashMap::with_capacity(*live);
                present.for_each(|i| {
                    map.insert(i, values[i as usize].clone());
                });
                map
            }
            Column::Str { ids, present, live } => {
                let mut map = HashMap::with_capacity(*live);
                present.for_each(|i| {
                    map.insert(i, intern.get(ids[i as usize]).clone());
                });
                map
            }
        }
    }
}

/// A pre-resolved column handle for efficient repeated node lookups.
///
/// Created by [`ColumnStore::column`]. Resolves the field name hash once so
/// that callers can look up many node IDs without re-hashing the field string
/// on every call — useful when iterating large label scans under a filter.
pub struct ColumnHandle<'a> {
    col: Option<&'a Column>,
    intern: &'a StrIntern,
}

impl<'a> ColumnHandle<'a> {
    /// Return the stored value for `node`, or `None` if the column is absent
    /// or the node has no value for this field.
    #[inline]
    pub fn get(&self, node: u32) -> Option<&'a Value> {
        self.col?.get(node, self.intern)
    }
}

/// Per-field typed columns. Homogeneous Int/Float/Bool/Str use a dense vec plus
/// presence bitmap; mixed-type fields and lists spill to a HashMap (slow path).
///
/// On-disk (V6) shape is still `HashMap<String, HashMap<u32, Value>>`.
#[derive(Debug, Default, Clone)]
pub struct ColumnStore {
    /// Each column behind its own `Arc`, so a clone copies the field list and
    /// a write copies the one column's chunk lists, never the others.
    cols: HashMap<String, Arc<Column>>,
    intern: StrIntern,
    /// Deleted prop keys for nodes whose value lives only in the V8 mmap'd base.
    /// Consulted by `ColumnsView::get` before falling through to the base section.
    /// Cleared at V8 snapshot merge (all data folds into the new base snapshot).
    pub(crate) prop_tombstones: CowMap<u32, BTreeSet<String>>,
}

impl ColumnStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, node: u32, field: &str, value: Value) {
        let ColumnStore { cols, intern, .. } = self;
        if let Some(col) = cols.get_mut(field) {
            Arc::make_mut(col).set(node, value, intern);
        } else {
            cols.insert(
                field.to_string(),
                Arc::new(Column::from_first(node, value, intern)),
            );
        }
    }

    pub fn get(&self, node: u32, field: &str) -> Option<&Value> {
        self.cols.get(field)?.get(node, &self.intern)
    }

    /// Return a pre-resolved column handle for `field`.
    ///
    /// Hashes the field name once so that repeated [`ColumnHandle::get`] calls
    /// pay only the inner node-id lookup cost, not the outer string hash.
    /// Returns a handle that always returns `None` when the field is absent.
    pub fn column(&self, field: &str) -> ColumnHandle<'_> {
        ColumnHandle {
            col: self.cols.get(field).map(|col| &**col),
            intern: &self.intern,
        }
    }

    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.cols.keys().map(|s| s.as_str())
    }

    /// Remove the value stored at `(node, field)` and return it, or `None` if absent.
    /// Prunes the column's inner map entry when it becomes empty.
    pub fn remove(&mut self, node: u32, field: &str) -> Option<Value> {
        let ColumnStore { cols, intern, .. } = self;
        let col = cols.get_mut(field)?;
        // Absent: nothing to remove, and nothing to copy.
        col.get(node, intern)?;
        let old = Arc::make_mut(col).remove(node, intern)?;
        if col.is_empty() {
            cols.remove(field);
        }
        Some(old)
    }

    /// Drop every field stored for `node`. Idempotent: a node with no remaining
    /// props (or an id that was never written) is a no-op. Used by `DeleteNode`
    /// after rule retraction and user-edge sweep. Field iteration order is not
    /// observable — the resulting store is identical regardless of HashMap order.
    pub fn remove_all(&mut self, node: u32) {
        let ColumnStore { cols, intern, .. } = self;
        cols.retain(|_, col| {
            // Only a column holding the node is written, and so copied.
            if col.get(node, intern).is_some() {
                Arc::make_mut(col).remove(node, intern);
            }
            !col.is_empty()
        });
    }

    /// True when `(node, field)` has been recorded as deleted in the prop
    /// tombstone map.  Used by `ColumnsView::get` to mask base-only props that
    /// were subsequently removed via the WAL.
    pub(crate) fn is_tombstoned(&self, node: u32, field: &str) -> bool {
        self.prop_tombstones
            .get(&node)
            .is_some_and(|fields| fields.contains(field))
    }

    /// Record that the base-only prop `(node, field)` was deleted via the WAL.
    ///
    /// This tombstone is consulted by `ColumnsView::get` before falling through
    /// to the archived base section, ensuring that removed base props are not
    /// resurrected by base fallback reads.  Consumed (cleared) at snapshot merge.
    pub fn record_prop_tombstone(&mut self, node: u32, field: &str) {
        self.prop_tombstones
            .get_or_insert_with(node, BTreeSet::new)
            .insert(field.to_string());
    }

    pub(crate) fn to_wire(&self) -> HashMap<String, HashMap<u32, Value>> {
        let mut cols = HashMap::with_capacity(self.cols.len());
        for (field, col) in &self.cols {
            cols.insert(field.clone(), col.to_map(&self.intern));
        }
        cols
    }

    fn from_wire(cols: HashMap<String, HashMap<u32, Value>>) -> Self {
        let mut store = Self::new();
        for (field, values) in cols {
            for (node, value) in values {
                store.set(node, &field, value);
            }
        }
        store
    }

    #[cfg(test)]
    fn is_mixed(&self, field: &str) -> bool {
        matches!(self.cols.get(field).map(|c| &**c), Some(Column::Mixed(_)))
    }

    /// V7 packed columns: intern table, then sorted field name + typed payload.
    ///
    /// Refuses a column holding a null value: the write path never lets one
    /// in, and this keeps one off disk if that check is ever bypassed.
    pub(crate) fn pack(&self, out: &mut Vec<u8>) -> StoreResult<()> {
        self.intern.pack(out);
        let mut fields: Vec<&String> = self.cols.keys().collect();
        fields.sort();
        push_u32(out, fields.len() as u32);
        for f in fields {
            push_str(out, f);
            self.cols[f].pack(&self.intern, out)?;
        }
        Ok(())
    }

    pub(crate) fn unpack(src: &[u8]) -> StoreResult<(Self, usize)> {
        let mut pos = 0usize;
        let intern = StrIntern::unpack(src, &mut pos)?;
        let n = read_u32(src, &mut pos)? as usize;
        let mut cols = HashMap::with_capacity(n);
        for _ in 0..n {
            let name = read_str(src, &mut pos)?;
            let col = Column::unpack(src, &mut pos)?;
            cols.insert(name, Arc::new(col));
        }
        Ok((
            Self {
                cols,
                intern,
                prop_tombstones: CowMap::new(),
            },
            pos,
        ))
    }
}

/// Read one packed column holding a pack tag 5-9 type — `Date`,
/// `LocalDateTime`, `DateTime`, `Duration` or `Binary` — starting at its tag
/// byte, as the node-to-value map those values make.
///
/// The snapshot encoder writes these columns into the COLUMNS section in the
/// `Mixed` shape, so the archived layout of the section does not move.
///
/// # Errors
/// [`GraphError::Corrupt`] for a truncated column, or for a tag that is not
/// one of 5-9.
pub(crate) fn unpack_typed_column_map(
    src: &[u8],
    pos: &mut usize,
) -> StoreResult<HashMap<u32, Value>> {
    let tag = src.get(*pos).copied();
    if !matches!(tag, Some(5..=9)) {
        return Err(GraphError::Corrupt {
            detail: format!("columns: expected a typed-value column tag 5-9, found {tag:?}"),
        });
    }
    let col = Column::unpack(src, pos)?;
    // No `Str` column comes through here, so nothing resolves against the
    // intern table.
    Ok(col.to_map(&StrIntern::default()))
}

/// The refusal a column encoder gives a null value (query-only, never stored).
fn null_in_column() -> GraphError {
    GraphError::Corrupt {
        detail: "refusing to encode a column whose property value holds a null".into(),
    }
}

impl Serialize for ColumnStore {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeStruct};
        let holds_null = self.cols.values().any(|c| match &**c {
            Column::Mixed(map) => map.values().any(Value::contains_null),
            _ => false,
        });
        if holds_null {
            return Err(S::Error::custom(null_in_column()));
        }
        let mut state = serializer.serialize_struct("ColumnStore", 1)?;
        state.serialize_field("cols", &self.to_wire())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ColumnStore {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            cols: HashMap<String, HashMap<u32, Value>>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self::from_wire(wire.cols))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    #[test]
    fn remove_returns_old_value_and_none_on_absent() {
        let mut c = ColumnStore::new();
        c.set(0, "name", Value::Str("ada".into()));
        assert_eq!(c.remove(0, "name"), Some(Value::Str("ada".into())));
        assert_eq!(c.get(0, "name"), None);
        // second remove: absent → None
        assert_eq!(c.remove(0, "name"), None);
        // completely absent field
        assert_eq!(c.remove(99, "absent"), None);
    }

    #[test]
    fn remove_prunes_empty_column_entry() {
        let mut c = ColumnStore::new();
        c.set(0, "x", Value::Int(1));
        c.set(1, "x", Value::Int(2));
        c.remove(0, "x");
        // one entry still present → column not pruned
        assert!(c.fields().any(|f| f == "x"));
        c.remove(1, "x");
        // now empty → column pruned
        assert!(!c.fields().any(|f| f == "x"));
    }

    #[test]
    fn remove_all_clears_every_field_and_is_noop_on_absent() {
        let mut c = ColumnStore::new();
        c.set(0, "name", Value::Str("ada".into()));
        c.set(0, "age", Value::Int(36));
        c.set(1, "name", Value::Str("bob".into()));
        c.remove_all(0);
        assert_eq!(c.get(0, "name"), None);
        assert_eq!(c.get(0, "age"), None);
        assert_eq!(c.get(1, "name"), Some(&Value::Str("bob".into())));
        // second call is a clean no-op (crash-window / already-cleared node)
        c.remove_all(0);
        assert_eq!(c.get(1, "name"), Some(&Value::Str("bob".into())));
        c.remove_all(99);
        assert_eq!(c.get(1, "name"), Some(&Value::Str("bob".into())));
    }

    #[test]
    fn set_get_overwrite_and_sparse_nodes() {
        let mut c = ColumnStore::new();
        c.set(0, "name", Value::Str("ada".into()));
        c.set(2, "name", Value::Str("bob".into())); // node 1 skipped: sparse
        c.set(0, "name", Value::Str("ada2".into())); // overwrite
        c.set(0, "age", Value::Int(36));
        assert_eq!(c.get(0, "name"), Some(&Value::Str("ada2".into())));
        assert_eq!(c.get(1, "name"), None);
        assert_eq!(c.get(2, "age"), None);
        let mut fields: Vec<_> = c.fields().collect();
        fields.sort();
        assert_eq!(fields, vec!["age", "name"]);
    }

    #[test]
    fn str_column_does_not_clone_on_get() {
        let mut c = ColumnStore::new();
        c.set(0, "name", Value::Str("ada".into()));
        assert_eq!(c.get(0, "name"), Some(&Value::Str("ada".into())));
        c.set(1, "name", Value::Str("ada".into()));
        assert!(std::ptr::eq(
            c.get(0, "name").unwrap(),
            c.get(1, "name").unwrap()
        ));
        c.set(0, "title", Value::Str("ada".into()));
        assert!(std::ptr::eq(
            c.get(0, "name").unwrap(),
            c.get(0, "title").unwrap()
        ));
    }

    #[test]
    fn mixed_type_column_round_trips() {
        let mut c = ColumnStore::new();
        c.set(0, "x", Value::Int(1));
        c.set(1, "x", Value::Str("a".into()));
        assert!(matches!(c.get(0, "x"), Some(&Value::Int(1))));
        assert!(matches!(c.get(1, "x"), Some(&Value::Str(_))));
        assert!(c.is_mixed("x"));
        c.remove(1, "x");
        assert!(c.is_mixed("x"));
        assert_eq!(c.get(0, "x"), Some(&Value::Int(1)));
    }

    #[test]
    fn list_and_type_change_spill_typed_scalars_stay_homogeneous() {
        let mut c = ColumnStore::new();
        c.set(0, "tags", Value::List(vec![Value::Int(1)]));
        c.set(1, "tags", Value::List(vec![Value::Int(2)]));
        assert!(c.is_mixed("tags"));
        assert_eq!(c.get(0, "tags"), Some(&Value::List(vec![Value::Int(1)])));

        c.set(0, "ok", Value::Bool(true));
        c.set(1, "ok", Value::Bool(false));
        assert!(!c.is_mixed("ok"));
        assert_eq!(c.get(0, "ok"), Some(&Value::Bool(true)));

        c.set(0, "score", Value::Float(1.5));
        c.set(64, "score", Value::Float(2.5));
        assert!(!c.is_mixed("score"));
        assert_eq!(c.get(63, "score"), None);
        assert_eq!(c.get(64, "score"), Some(&Value::Float(2.5)));

        c.set(0, "n", Value::Int(1));
        c.set(64, "n", Value::Int(2));
        assert_eq!(c.get(0, "n"), Some(&Value::Int(1)));
        assert_eq!(c.get(64, "n"), Some(&Value::Int(2)));

        c.set(0, "flip", Value::Int(1));
        c.set(0, "flip", Value::Str("a".into()));
        assert!(c.is_mixed("flip"));
        assert_eq!(c.get(0, "flip"), Some(&Value::Str("a".into())));
    }

    #[test]
    fn column_handle_matches_get() {
        let mut c = ColumnStore::new();
        c.set(0, "name", Value::Str("ada".into()));
        c.set(2, "age", Value::Int(36));
        let name = c.column("name");
        let age = c.column("age");
        let missing = c.column("nope");
        assert_eq!(name.get(0), c.get(0, "name"));
        assert_eq!(name.get(1), None);
        assert_eq!(age.get(2), Some(&Value::Int(36)));
        assert_eq!(missing.get(0), None);
    }

    #[test]
    fn a_null_value_is_never_encoded_into_a_column() {
        // The write path refuses a null before it reaches the store. If one
        // got into a column anyway, both column encoders refuse it: `pack`
        // (the v7/v8 snapshot form) and the serde form.
        let mut store = ColumnStore::default();
        store.set(0, "tags", Value::List(vec![Value::Int(1), Value::Null]));
        assert!(store.is_mixed("tags"));
        let mut buf = Vec::new();
        let err = store.pack(&mut buf).expect_err("pack refuses a null");
        assert!(err.to_string().contains("null"), "{err}");
        assert!(bincode::serialize(&store).is_err());
    }

    #[test]
    fn every_storable_value_variant_round_trips_through_both_column_encoders() {
        let mut store = ColumnStore::default();
        for (node, v) in crate::types::pinned::storable_values()
            .into_iter()
            .enumerate()
        {
            store.set(node as u32, "mixed", v);
        }
        let check = |back: &ColumnStore| {
            for (node, v) in crate::types::pinned::storable_values().iter().enumerate() {
                assert_eq!(back.get(node as u32, "mixed"), Some(v));
            }
        };
        let mut buf = Vec::new();
        store.pack(&mut buf).unwrap();
        let (back, used) = ColumnStore::unpack(&buf).unwrap();
        assert_eq!(used, buf.len());
        check(&back);
        let back: ColumnStore = bincode::deserialize(&bincode::serialize(&store).unwrap()).unwrap();
        check(&back);
    }

    #[test]
    fn serde_wire_is_nested_hashmap() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wire {
            cols: HashMap<String, HashMap<u32, Value>>,
        }

        let mut cols = HashMap::new();
        cols.insert("age".into(), HashMap::from([(0, Value::Int(30))]));
        cols.insert(
            "name".into(),
            HashMap::from([(1, Value::Str("ada".into()))]),
        );
        cols.insert(
            "mixed".into(),
            HashMap::from([(0, Value::Int(1)), (1, Value::Str("x".into()))]),
        );
        cols.insert(
            "tags".into(),
            HashMap::from([(2, Value::List(vec![Value::Int(1)]))]),
        );
        let wire = Wire { cols };

        let encoded = bincode::serialize(&wire).unwrap();
        let store: ColumnStore = bincode::deserialize(&encoded).unwrap();
        assert_eq!(store.get(0, "age"), Some(&Value::Int(30)));
        assert_eq!(store.get(1, "name"), Some(&Value::Str("ada".into())));
        assert_eq!(store.get(0, "mixed"), Some(&Value::Int(1)));
        assert_eq!(store.get(1, "mixed"), Some(&Value::Str("x".into())));
        assert_eq!(
            store.get(2, "tags"),
            Some(&Value::List(vec![Value::Int(1)]))
        );
        assert!(store.is_mixed("mixed"));
        assert!(store.is_mixed("tags"));
        assert!(!store.is_mixed("age"));
        assert!(!store.is_mixed("name"));

        let roundtrip: Wire = bincode::deserialize(&bincode::serialize(&store).unwrap()).unwrap();
        assert_eq!(roundtrip.cols["age"][&0], Value::Int(30));
        assert_eq!(roundtrip.cols["name"][&1], Value::Str("ada".into()));
        assert_eq!(roundtrip.cols["mixed"][&0], Value::Int(1));
        assert_eq!(roundtrip.cols["mixed"][&1], Value::Str("x".into()));
        assert_eq!(roundtrip.cols["tags"][&2], Value::List(vec![Value::Int(1)]));
    }

    fn dt(micros_utc: i64, offset_seconds: i32) -> Value {
        Value::DateTime {
            micros_utc,
            offset_seconds,
        }
    }

    fn dur(months: i32, days: i32, nanos: i64) -> Value {
        Value::Duration {
            months,
            days,
            nanos,
        }
    }

    fn pack_round_trip(col: &Column, intern: &StrIntern) -> Column {
        let mut buf = Vec::new();
        col.pack(intern, &mut buf).unwrap();
        let mut pos = 0;
        let back = Column::unpack(&buf, &mut pos).unwrap();
        assert_eq!(pos, buf.len());
        back
    }

    #[test]
    fn test_column_date_pack_unpack() {
        let mut intern = StrIntern::default();
        let mut col = Column::from_first(0, Value::Date(100), &mut intern);
        col.set(1, Value::Date(200), &mut intern);
        col.set(2, Value::Date(-300), &mut intern);
        assert!(matches!(
            col,
            Column::Fixed {
                kind: Fixed::Date,
                ..
            }
        ));
        let back = pack_round_trip(&col, &intern);
        assert!(matches!(
            back,
            Column::Fixed {
                kind: Fixed::Date,
                ..
            }
        ));
        assert_eq!(back.get(0, &intern), Some(&Value::Date(100)));
        assert_eq!(back.get(1, &intern), Some(&Value::Date(200)));
        assert_eq!(back.get(2, &intern), Some(&Value::Date(-300)));
    }

    #[test]
    fn test_column_binary_pack_unpack() {
        let mut intern = StrIntern::default();
        let mut col = Column::from_first(0, Value::Binary(vec![1, 2, 3]), &mut intern);
        col.set(1, Value::Binary(vec![4, 5]), &mut intern);
        col.set(3, Value::Binary(Vec::new()), &mut intern);
        let back = pack_round_trip(&col, &intern);
        assert!(matches!(back, Column::Binary { .. }));
        assert_eq!(back.get(0, &intern), Some(&Value::Binary(vec![1, 2, 3])));
        assert_eq!(back.get(1, &intern), Some(&Value::Binary(vec![4, 5])));
        assert_eq!(back.get(2, &intern), None);
        assert_eq!(back.get(3, &intern), Some(&Value::Binary(Vec::new())));
    }

    #[test]
    fn column_ldt_dt_dur_pack_unpack_keep_every_field() {
        let mut intern = StrIntern::default();
        let cases = [
            vec![
                Value::LocalDateTime(i64::MIN),
                Value::LocalDateTime(i64::MAX),
            ],
            vec![dt(i64::MIN, -64_800), dt(i64::MAX, 64_800), dt(5, 0)],
            vec![dur(i32::MIN, i32::MAX, i64::MIN), dur(-1, 1, i64::MAX)],
        ];
        for values in cases {
            let mut col = Column::from_first(0, values[0].clone(), &mut intern);
            for (i, v) in values.iter().enumerate().skip(1) {
                col.set(i as u32 * 2, v.clone(), &mut intern); // sparse
            }
            let back = pack_round_trip(&col, &intern);
            for (i, v) in values.iter().enumerate() {
                assert_eq!(back.get(i as u32 * 2, &intern), Some(v));
            }
            assert_eq!(back.get(1, &intern), None);
        }
    }

    #[test]
    fn column_new_types_overwrite_remove_and_prune() {
        let mut intern = StrIntern::default();
        let values = [
            Value::Date(1),
            Value::LocalDateTime(2),
            dt(3, 3600),
            dur(1, 2, 3),
            Value::Binary(vec![9]),
        ];
        for v in values {
            let mut col = Column::from_first(0, v.clone(), &mut intern);
            col.set(1, v.clone(), &mut intern);
            col.set(0, v.clone(), &mut intern); // overwrite keeps live count
            assert!(!col.is_empty());
            assert_eq!(col.remove(0, &intern), Some(v.clone()));
            assert_eq!(col.remove(0, &intern), None);
            assert_eq!(col.get(0, &intern), None);
            assert_eq!(col.get(1, &intern), Some(&v));
            assert_eq!(col.remove(1, &intern), Some(v));
            assert!(col.is_empty());
        }
    }

    #[test]
    fn test_column_date_promotes_to_mixed_on_type_mismatch() {
        let mut intern = StrIntern::default();
        let mut col = Column::from_first(0, Value::Date(100), &mut intern);
        col.set(1, Value::LocalDateTime(12345678), &mut intern);
        assert!(matches!(col, Column::Mixed(_)));
        assert_eq!(col.get(0, &intern), Some(&Value::Date(100)));
        assert_eq!(col.get(1, &intern), Some(&Value::LocalDateTime(12345678)));
    }

    #[test]
    fn promotion_to_mixed_preserves_every_value_exactly() {
        let mut intern = StrIntern::default();
        let samples = [
            Value::Date(-719_528),
            Value::LocalDateTime(-1),
            dt(1_705_321_496_789_012, -19_800),
            dur(14, -3, 3_723_000_000_000),
            Value::Binary(vec![0, 255, 7]),
            Value::Int(7),
            Value::Str("s".into()),
        ];
        for (a, first) in samples.iter().enumerate() {
            for (b, other) in samples.iter().enumerate() {
                if a == b {
                    continue;
                }
                let mut col = Column::from_first(0, first.clone(), &mut intern);
                col.set(1, first.clone(), &mut intern);
                col.set(2, other.clone(), &mut intern);
                assert!(matches!(col, Column::Mixed(_)), "{first:?} then {other:?}");
                assert_eq!(col.get(0, &intern), Some(first));
                assert_eq!(col.get(1, &intern), Some(first));
                assert_eq!(col.get(2, &intern), Some(other));
                let back = pack_round_trip(&col, &intern);
                assert_eq!(back.get(0, &intern), Some(first));
                assert_eq!(back.get(2, &intern), Some(other));
            }
        }
    }

    #[test]
    fn column_store_new_types_round_trip_pack_and_wire() {
        let mut c = ColumnStore::new();
        c.set(0, "d", Value::Date(19_737));
        c.set(1, "d", Value::Date(0));
        c.set(0, "ts", dt(1_705_321_496_789_012, 19_800));
        c.set(2, "len", dur(1, 2, 3));
        c.set(0, "blob", Value::Binary(vec![1, 2]));
        c.set(0, "mix", Value::Date(1));
        c.set(1, "mix", Value::Binary(vec![3]));
        let mut buf = Vec::new();
        c.pack(&mut buf).unwrap();
        let (back, consumed) = ColumnStore::unpack(&buf).unwrap();
        assert_eq!(consumed, buf.len());
        let via_serde: ColumnStore =
            bincode::deserialize(&bincode::serialize(&c).unwrap()).unwrap();
        for store in [&back, &via_serde] {
            assert_eq!(store.get(0, "d"), Some(&Value::Date(19_737)));
            assert_eq!(store.get(1, "d"), Some(&Value::Date(0)));
            assert_eq!(store.get(0, "ts"), Some(&dt(1_705_321_496_789_012, 19_800)));
            assert_eq!(store.get(2, "len"), Some(&dur(1, 2, 3)));
            assert_eq!(store.get(0, "blob"), Some(&Value::Binary(vec![1, 2])));
            assert_eq!(store.get(1, "mix"), Some(&Value::Binary(vec![3])));
            assert!(store.is_mixed("mix"));
            assert!(!store.is_mixed("d"));
        }
    }

    #[test]
    fn truncated_new_type_columns_are_corrupt_not_panics() {
        let mut intern = StrIntern::default();
        for v in [
            Value::Date(1),
            Value::LocalDateTime(1),
            dt(1, 1),
            dur(1, 1, 1),
            Value::Binary(vec![1, 2, 3]),
        ] {
            let col = Column::from_first(0, v, &mut intern);
            let mut buf = Vec::new();
            col.pack(&intern, &mut buf).unwrap();
            for cut in 1..buf.len() {
                let mut pos = 0;
                assert!(Column::unpack(&buf[..cut], &mut pos).is_err(), "cut {cut}");
            }
        }
        // A count far beyond the buffer is an error, not an allocation.
        for tag in 5..=9u8 {
            let mut pos = 0;
            let huge = [tag, 0xff, 0xff, 0xff, 0xff];
            assert!(Column::unpack(&huge, &mut pos).is_err());
        }
    }

    #[test]
    fn pack_roundtrip_typed_mixed_and_intern() {
        let mut c = ColumnStore::new();
        c.set(0, "name", Value::Str("ada".into()));
        c.set(1, "name", Value::Str("ada".into()));
        c.set(0, "age", Value::Int(36));
        c.set(0, "ok", Value::Bool(true));
        c.set(2, "score", Value::Float(1.5));
        c.set(0, "mix", Value::Int(1));
        c.set(1, "mix", Value::Str("x".into()));
        c.set(3, "tags", Value::List(vec![Value::Int(1)]));
        let mut buf = Vec::new();
        c.pack(&mut buf).unwrap();
        let (back, consumed) = ColumnStore::unpack(&buf).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(back.get(0, "name"), Some(&Value::Str("ada".into())));
        assert!(std::ptr::eq(
            back.get(0, "name").unwrap(),
            back.get(1, "name").unwrap()
        ));
        assert_eq!(back.get(0, "age"), Some(&Value::Int(36)));
        assert_eq!(back.get(0, "ok"), Some(&Value::Bool(true)));
        assert_eq!(back.get(2, "score"), Some(&Value::Float(1.5)));
        assert_eq!(back.get(0, "mix"), Some(&Value::Int(1)));
        assert_eq!(back.get(1, "mix"), Some(&Value::Str("x".into())));
        assert_eq!(back.get(3, "tags"), Some(&Value::List(vec![Value::Int(1)])));
        assert!(back.is_mixed("mix"));
        assert!(back.is_mixed("tags"));
    }

    /// Ledger row 126: the reader's fold holds a clone of the column store and
    /// the writer goes on writing its own. Every kind of write to the writer's
    /// copy — across chunk boundaries, typed and mixed columns, a promotion to
    /// mixed, removals, a node's whole row, tombstones, new interned strings —
    /// leaves the clone exactly as it was, with real hashes and with every
    /// string's hash the same.
    #[test]
    fn a_clone_keeps_its_contents_while_the_original_is_written() {
        for forced in [None, Some(7_u64)] {
            crate::cow::FORCED_HASH.with(|f| f.set(forced));
            let n = 3_000u32;
            let mut live = ColumnStore::new();
            for i in 0..n {
                live.set(i, "name", Value::Str(format!("n{i}")));
                live.set(i, "age", Value::Int(i64::from(i)));
                if i % 3 == 0 {
                    live.set(i, "tags", Value::List(vec![Value::Int(i64::from(i))]));
                }
            }
            let frozen = live.clone();
            for i in (0..n).step_by(7) {
                live.set(i, "name", Value::Str(format!("renamed {i}")));
                live.set(i, "age", Value::Int(-1));
            }
            live.set(5, "age", Value::Str("promoted".into()));
            live.remove(14, "name");
            live.remove_all(21);
            live.set(n + 500, "name", Value::Str("n1".into()));
            live.record_prop_tombstone(3, "gone");
            for i in 0..n {
                assert_eq!(frozen.get(i, "name"), Some(&Value::Str(format!("n{i}"))));
                assert_eq!(frozen.get(i, "age"), Some(&Value::Int(i64::from(i))));
                let tags = (i % 3 == 0).then(|| Value::List(vec![Value::Int(i64::from(i))]));
                assert_eq!(frozen.get(i, "tags"), tags.as_ref());
            }
            assert_eq!(frozen.get(n + 500, "name"), None);
            assert!(!frozen.is_mixed("age") && live.is_mixed("age"));
            assert!(!frozen.is_tombstoned(3, "gone") && live.is_tombstoned(3, "gone"));
            assert_eq!(live.get(7, "name"), Some(&Value::Str("renamed 7".into())));
            assert_eq!(live.get(14, "name"), None);
            assert_eq!(live.get(21, "age"), None);
            assert!(std::ptr::eq(
                live.get(n + 500, "name").unwrap(),
                live.get(1, "name").unwrap()
            ));
            let (back, _) = ColumnStore::unpack(&{
                let mut b = Vec::new();
                live.pack(&mut b).unwrap();
                b
            })
            .unwrap();
            for i in 0..n {
                assert_eq!(back.get(i, "name"), live.get(i, "name"));
                assert_eq!(back.get(i, "tags"), live.get(i, "tags"));
            }
            crate::cow::FORCED_HASH.with(|f| f.set(None));
        }
    }
}
