use crate::pack::{
    push_f64s, push_i64s, push_str, push_u32, push_u32s, read_exact, read_f64s, read_i64s,
    read_str, read_u32, read_u32s,
};
use crate::types::Result as StoreResult;
use crate::types::{GraphError, Value};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;

/// Presence bitmap: bit i is set iff node i has a value. Unset bits are nulls.
#[derive(Debug, Default, Clone)]
struct Bitmap {
    bits: Vec<u64>,
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
            self.bits.resize(word + 1, 0);
        }
        let mask = 1u64 << bit;
        let newly = self.bits[word] & mask == 0;
        self.bits[word] |= mask;
        newly
    }

    /// Clear bit `i`. Returns true if it was previously set.
    fn clear(&mut self, i: u32) -> bool {
        let i = i as usize;
        let word = i / 64;
        let bit = i % 64;
        let Some(w) = self.bits.get_mut(word) else {
            return false;
        };
        let mask = 1u64 << bit;
        let was = *w & mask != 0;
        *w &= !mask;
        was
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
        for w in &self.bits {
            out.extend_from_slice(&w.to_le_bytes());
        }
    }

    fn unpack(src: &[u8], pos: &mut usize) -> StoreResult<Self> {
        let n = read_u32(src, pos)? as usize;
        let bytes = read_exact(src, pos, n.saturating_mul(8))?;
        let mut bits = Vec::with_capacity(n);
        for chunk in bytes.chunks_exact(8) {
            bits.push(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        Ok(Self { bits })
    }
}

/// Append-only intern of `Value::Str`. Homogeneous string columns store ids here.
#[derive(Debug, Default, Clone)]
struct StrIntern {
    to_id: HashMap<String, u32>,
    values: Vec<Value>,
}

impl StrIntern {
    fn intern(&mut self, s: String) -> u32 {
        use std::collections::hash_map::Entry;
        match self.to_id.entry(s) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(e) => {
                let id = self.values.len() as u32;
                let cloned = e.key().clone();
                e.insert(id);
                self.values.push(Value::Str(cloned));
                id
            }
        }
    }

    fn get(&self, id: u32) -> &Value {
        &self.values[id as usize]
    }

    fn pack(&self, out: &mut Vec<u8>) {
        push_u32(out, self.values.len() as u32);
        for v in &self.values {
            let Value::Str(s) = v else {
                unreachable!("StrIntern values are always Value::Str");
            };
            push_str(out, s);
        }
    }

    fn unpack(src: &[u8], pos: &mut usize) -> StoreResult<Self> {
        let n = read_u32(src, pos)? as usize;
        let mut intern = Self::default();
        intern.values.reserve(n);
        intern.to_id.reserve(n);
        for i in 0..n {
            let s = read_str(src, pos)?;
            intern.to_id.insert(s.clone(), i as u32);
            intern.values.push(Value::Str(s));
        }
        Ok(intern)
    }
}

fn grow<T: Clone>(data: &mut Vec<T>, adapter: &mut Vec<Value>, node: u32, fill: T, dummy: Value) {
    let n = node as usize + 1;
    if data.len() < n {
        data.resize(n, fill);
        adapter.resize(n, dummy);
    }
}

#[derive(Debug, Clone)]
enum Column {
    Int {
        data: Vec<i64>,
        present: Bitmap,
        adapter: Vec<Value>,
        live: usize,
    },
    Float {
        data: Vec<f64>,
        present: Bitmap,
        adapter: Vec<Value>,
        live: usize,
    },
    Bool {
        data: Vec<bool>,
        present: Bitmap,
        adapter: Vec<Value>,
        live: usize,
    },
    Str {
        ids: Vec<u32>,
        present: Bitmap,
        live: usize,
    },
    /// Slow path: mixed-type fields and `Value::List`. Never demoted back to homogeneous.
    Mixed(HashMap<u32, Value>),
}

impl Column {
    fn from_first(node: u32, value: Value, intern: &mut StrIntern) -> Self {
        let mut col = match &value {
            Value::Int(_) => Column::Int {
                data: Vec::new(),
                present: Bitmap::default(),
                adapter: Vec::new(),
                live: 0,
            },
            Value::Float(_) => Column::Float {
                data: Vec::new(),
                present: Bitmap::default(),
                adapter: Vec::new(),
                live: 0,
            },
            Value::Bool(_) => Column::Bool {
                data: Vec::new(),
                present: Bitmap::default(),
                adapter: Vec::new(),
                live: 0,
            },
            Value::Str(_) => Column::Str {
                ids: Vec::new(),
                present: Bitmap::default(),
                live: 0,
            },
            Value::List(_) => Column::Mixed(HashMap::new()),
        };
        col.set(node, value, intern);
        col
    }

    fn accepts(&self, value: &Value) -> bool {
        matches!(
            (self, value),
            (Column::Int { .. }, Value::Int(_))
                | (Column::Float { .. }, Value::Float(_))
                | (Column::Bool { .. }, Value::Bool(_))
                | (Column::Str { .. }, Value::Str(_))
                | (Column::Mixed(_), _)
        )
    }

    fn set(&mut self, node: u32, value: Value, intern: &mut StrIntern) {
        if !self.accepts(&value) {
            let mut map = self.take_map(intern);
            map.insert(node, value);
            *self = Column::Mixed(map);
            return;
        }
        match (self, value) {
            (
                Column::Int {
                    data,
                    present,
                    adapter,
                    live,
                },
                Value::Int(v),
            ) => {
                grow(data, adapter, node, 0, Value::Int(0));
                data[node as usize] = v;
                adapter[node as usize] = Value::Int(v);
                if present.set(node) {
                    *live += 1;
                }
            }
            (
                Column::Float {
                    data,
                    present,
                    adapter,
                    live,
                },
                Value::Float(v),
            ) => {
                grow(data, adapter, node, 0.0, Value::Float(0.0));
                data[node as usize] = v;
                adapter[node as usize] = Value::Float(v);
                if present.set(node) {
                    *live += 1;
                }
            }
            (
                Column::Bool {
                    data,
                    present,
                    adapter,
                    live,
                },
                Value::Bool(v),
            ) => {
                grow(data, adapter, node, false, Value::Bool(false));
                data[node as usize] = v;
                adapter[node as usize] = Value::Bool(v);
                if present.set(node) {
                    *live += 1;
                }
            }
            (Column::Str { ids, present, live }, Value::Str(s)) => {
                let id = intern.intern(s);
                let n = node as usize + 1;
                if ids.len() < n {
                    ids.resize(n, 0);
                }
                ids[node as usize] = id;
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
            Column::Int {
                present, adapter, ..
            }
            | Column::Float {
                present, adapter, ..
            }
            | Column::Bool {
                present, adapter, ..
            } => {
                if present.contains(node) {
                    Some(&adapter[node as usize])
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
            Column::Int {
                data,
                present,
                live,
                ..
            } => {
                if !present.clear(node) {
                    return None;
                }
                *live -= 1;
                Some(Value::Int(data[node as usize]))
            }
            Column::Float {
                data,
                present,
                live,
                ..
            } => {
                if !present.clear(node) {
                    return None;
                }
                *live -= 1;
                Some(Value::Float(data[node as usize]))
            }
            Column::Bool {
                data,
                present,
                live,
                ..
            } => {
                if !present.clear(node) {
                    return None;
                }
                *live -= 1;
                Some(Value::Bool(data[node as usize]))
            }
            Column::Str { ids, present, live } => {
                if !present.clear(node) {
                    return None;
                }
                *live -= 1;
                Some(intern.get(ids[node as usize]).clone())
            }
            Column::Mixed(map) => map.remove(&node),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Column::Mixed(map) => map.is_empty(),
            Column::Int { live, .. }
            | Column::Float { live, .. }
            | Column::Bool { live, .. }
            | Column::Str { live, .. } => *live == 0,
        }
    }

    fn take_map(&mut self, intern: &StrIntern) -> HashMap<u32, Value> {
        match std::mem::replace(self, Column::Mixed(HashMap::new())) {
            Column::Mixed(map) => map,
            other => other.to_map(intern),
        }
    }

    fn pack(&self, intern: &StrIntern, out: &mut Vec<u8>) {
        match self {
            Column::Int { data, present, .. } => {
                out.push(0);
                push_i64s(out, data);
                present.pack(out);
            }
            Column::Float { data, present, .. } => {
                out.push(1);
                push_f64s(out, data);
                present.pack(out);
            }
            Column::Bool { data, present, .. } => {
                out.push(2);
                push_u32(out, data.len() as u32);
                out.reserve(data.len());
                for b in data {
                    out.push(u8::from(*b));
                }
                present.pack(out);
            }
            Column::Str { ids, present, .. } => {
                out.push(3);
                push_u32s(out, ids);
                present.pack(out);
            }
            Column::Mixed(map) => {
                out.push(4);
                let blob = bincode::serialize(map).expect("mixed column serialize cannot fail");
                push_u32(out, blob.len() as u32);
                out.extend_from_slice(&blob);
            }
        }
        let _ = intern;
    }

    fn unpack(src: &[u8], pos: &mut usize) -> StoreResult<Self> {
        let tag = *read_exact(src, pos, 1)?.first().unwrap();
        match tag {
            0 => {
                let data = read_i64s(src, pos)?;
                let present = Bitmap::unpack(src, pos)?;
                let live = present.live_count();
                let adapter = data.iter().copied().map(Value::Int).collect();
                Ok(Column::Int {
                    data,
                    present,
                    adapter,
                    live,
                })
            }
            1 => {
                let data = read_f64s(src, pos)?;
                let present = Bitmap::unpack(src, pos)?;
                let live = present.live_count();
                let adapter = data.iter().copied().map(Value::Float).collect();
                Ok(Column::Float {
                    data,
                    present,
                    adapter,
                    live,
                })
            }
            2 => {
                let n = read_u32(src, pos)? as usize;
                let bytes = read_exact(src, pos, n)?;
                let data: Vec<bool> = bytes.iter().map(|&b| b != 0).collect();
                let present = Bitmap::unpack(src, pos)?;
                let live = present.live_count();
                let adapter = data.iter().copied().map(Value::Bool).collect();
                Ok(Column::Bool {
                    data,
                    present,
                    adapter,
                    live,
                })
            }
            3 => {
                let ids = read_u32s(src, pos)?;
                let present = Bitmap::unpack(src, pos)?;
                let live = present.live_count();
                Ok(Column::Str { ids, present, live })
            }
            4 => {
                let n = read_u32(src, pos)? as usize;
                let blob = read_exact(src, pos, n)?;
                let map: HashMap<u32, Value> =
                    bincode::deserialize(blob).map_err(|e| GraphError::Corrupt {
                        detail: format!("snapshot: mixed column: {e}"),
                    })?;
                Ok(Column::Mixed(map))
            }
            other => Err(GraphError::Corrupt {
                detail: format!("snapshot: unknown column tag {other}"),
            }),
        }
    }

    fn to_map(&self, intern: &StrIntern) -> HashMap<u32, Value> {
        match self {
            Column::Mixed(map) => map.clone(),
            Column::Int {
                data,
                present,
                live,
                ..
            } => {
                let mut map = HashMap::with_capacity(*live);
                present.for_each(|i| {
                    map.insert(i, Value::Int(data[i as usize]));
                });
                map
            }
            Column::Float {
                data,
                present,
                live,
                ..
            } => {
                let mut map = HashMap::with_capacity(*live);
                present.for_each(|i| {
                    map.insert(i, Value::Float(data[i as usize]));
                });
                map
            }
            Column::Bool {
                data,
                present,
                live,
                ..
            } => {
                let mut map = HashMap::with_capacity(*live);
                present.for_each(|i| {
                    map.insert(i, Value::Bool(data[i as usize]));
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
    cols: HashMap<String, Column>,
    intern: StrIntern,
}

impl ColumnStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, node: u32, field: &str, value: Value) {
        let ColumnStore { cols, intern } = self;
        if let Some(col) = cols.get_mut(field) {
            col.set(node, value, intern);
        } else {
            cols.insert(field.to_string(), Column::from_first(node, value, intern));
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
            col: self.cols.get(field),
            intern: &self.intern,
        }
    }

    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.cols.keys().map(|s| s.as_str())
    }

    /// Remove the value stored at `(node, field)` and return it, or `None` if absent.
    /// Prunes the column's inner map entry when it becomes empty.
    pub fn remove(&mut self, node: u32, field: &str) -> Option<Value> {
        let ColumnStore { cols, intern } = self;
        let old = cols.get_mut(field)?.remove(node, intern)?;
        if cols.get(field).is_some_and(Column::is_empty) {
            cols.remove(field);
        }
        Some(old)
    }

    /// Drop every field stored for `node`. Idempotent: a node with no remaining
    /// props (or an id that was never written) is a no-op. Used by `DeleteNode`
    /// after rule retraction and user-edge sweep. Field iteration order is not
    /// observable — the resulting store is identical regardless of HashMap order.
    pub fn remove_all(&mut self, node: u32) {
        let ColumnStore { cols, intern } = self;
        cols.retain(|_, col| {
            col.remove(node, intern);
            !col.is_empty()
        });
    }

    fn to_wire(&self) -> HashMap<String, HashMap<u32, Value>> {
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
        matches!(self.cols.get(field), Some(Column::Mixed(_)))
    }

    /// V7 packed columns: intern table, then sorted field name + typed payload.
    pub(crate) fn pack(&self, out: &mut Vec<u8>) {
        self.intern.pack(out);
        let mut fields: Vec<&String> = self.cols.keys().collect();
        fields.sort();
        push_u32(out, fields.len() as u32);
        for f in fields {
            push_str(out, f);
            self.cols[f].pack(&self.intern, out);
        }
    }

    pub(crate) fn unpack(src: &[u8]) -> StoreResult<(Self, usize)> {
        let mut pos = 0usize;
        let intern = StrIntern::unpack(src, &mut pos)?;
        let n = read_u32(src, &mut pos)? as usize;
        let mut cols = HashMap::with_capacity(n);
        for _ in 0..n {
            let name = read_str(src, &mut pos)?;
            let col = Column::unpack(src, &mut pos)?;
            cols.insert(name, col);
        }
        Ok((Self { cols, intern }, pos))
    }
}

impl Serialize for ColumnStore {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
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
        c.pack(&mut buf);
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
}
