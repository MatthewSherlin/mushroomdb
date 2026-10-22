//! The one index over scalar node properties: equality and range.
//!
//! A declared `(label, field)` pair keeps a sorted map from each value's
//! [`ValueKey`] to the nodes holding it. Equality is a point lookup in that
//! map; a range or a string prefix is a `BTreeMap::range` over it — the same
//! structure answers both (0.10 W2 TY-D1, "unify"). What a pair was declared
//! as is its [`IndexKind`]: `EnableIndex` declares an equality index, and
//! `EnableRangeIndex` a named range index. A range index also answers
//! equality, so a pair carrying either is maintained the same way.
//!
//! Only scalar values are indexable; list-, map- and `Duration`-valued
//! properties have no [`ValueKey`] and are skipped. A per-node reverse map
//! records the current key so an update or delete removes the stale entry in
//! `O(log n)` without knowing the previous value.
//!
//! **Answers equal the scan's.** [`UnifiedIndex::lookup`] returns exactly the
//! nodes whose value `core-query`'s `values_equal` finds equal, and
//! [`UnifiedIndex::lookup_range`] exactly those its `eval_cmp` puts within the
//! bounds. Keys are stored by variant, so the lookups bridge what the keys do
//! not: `Int` and `Float` are one numeric class (`2 = 2.0`), compared as `f64`
//! by total order (`-0.0 < 0.0`, `NaN` above `+inf`); `Bool` is not ordered;
//! two `DateTime`s at one instant are equal yet ordered by offset.
//!
//! Nothing here is persisted: declarations replay from the WAL and the
//! postings rebuild at open ([`UnifiedIndex::rebuild_all`]).

use crate::cow::{CowMap, IdSet};
use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::types::{Value, ValueKey};
use crate::v8::seam::ColumnsView;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

/// What a maintained `(label, field)` pair was declared as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexKind {
    /// Declared by `EnableIndex` only: exact-match lookups.
    Equality,
    /// A named range index is declared on the pair: ranges, prefixes and
    /// exact matches.
    Range,
}

type Pair = (String, String);
/// Value → the nodes holding it. Copy-on-write in leaves, as are the reverse
/// and offset maps: the write path shares the index with the reader's fold,
/// and the first indexed write after a fold copies the leaves it writes, not
/// the index (ledger row 126).
type Postings = CowMap<ValueKey, IdSet>;

/// What a layered index answers beneath its own entries: a frozen index, with
/// the nodes whose entries there may be stale left out.
#[derive(Debug, Clone)]
struct IndexUnder {
    base: std::sync::Arc<UnifiedIndex>,
    stale: BTreeSet<u32>,
}

/// Equality and range index over declared `(label, field)` pairs.
#[derive(Debug, Clone, Default)]
pub struct UnifiedIndex {
    /// Every maintained pair, whatever it was declared as. A pair not present
    /// here is not indexed.
    enabled: BTreeSet<Pair>,
    /// `(label, field)` → value → set of node ids holding that value.
    forward: BTreeMap<Pair, Postings>,
    /// `(label, field)` → id → the node's current indexed key, so an update or
    /// delete can drop the stale forward entry without the old value. Built
    /// with the postings, at open too, so no write pays for it.
    reverse: BTreeMap<Pair, CowMap<u32, ValueKey>>,
    /// `(label, field)` → id → UTC offset, for the nodes whose value is a
    /// `DateTime` with a non-zero offset. The key holds the instant only — two
    /// `DateTime`s at one instant are equal — but `<` orders such a tie by
    /// offset, so a range needs it to answer as a scan does.
    offsets: BTreeMap<Pair, CowMap<u32, i32>>,
    /// The kind of every maintained pair: `Range` while a range index is
    /// declared on it, else `Equality`.
    kind: BTreeMap<Pair, IndexKind>,
    /// Pairs declared by `EnableIndex`. Kept apart from the range declarations
    /// so that dropping a range index over a pair that also has an equality
    /// index leaves the equality index in place.
    equality: BTreeSet<Pair>,
    /// Range index name → the pair it covers.
    range_names: BTreeMap<String, Pair>,
    /// `Some` only on a read-only layered index (see [`Self::layered_over`]):
    /// [`Self::lookup`] then also answers from `base`, minus the `stale` nodes,
    /// whose current entries are this index's own.
    under: Option<std::sync::Arc<IndexUnder>>,
}

fn pair(label: &str, field: &str) -> Pair {
    (label.to_string(), field.to_string())
}

impl UnifiedIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// A read-only index that answers [`Self::lookup`] as `base` would after
    /// the nodes in `stale` were re-indexed: `base`'s entries for every other
    /// node, plus this index's own, which the caller writes with [`Self::set`]
    /// for each stale node's current values. Declares exactly `base`'s pairs.
    ///
    /// Built per reader snapshot so a snapshot can serve indexed equality
    /// lookups without copying the frozen index. Only [`Self::lookup`] (what a
    /// query's `nodes_with_prop` calls) and the declaration queries see the
    /// base; the raw-key, range and prefix lookups see the layer's own entries.
    pub fn layered_over(base: std::sync::Arc<UnifiedIndex>, stale: BTreeSet<u32>) -> Self {
        Self {
            enabled: base.enabled.clone(),
            kind: base.kind.clone(),
            equality: base.equality.clone(),
            range_names: base.range_names.clone(),
            under: Some(std::sync::Arc::new(IndexUnder { base, stale })),
            ..Self::default()
        }
    }

    /// On a layered index: stop answering `id` from the base, and drop this
    /// layer's own entries for it, so the caller can [`Self::set`] its current
    /// values. Extends a layer to a longer tail without rebuilding it. No-op
    /// on an unlayered index.
    pub fn restale(&mut self, id: u32) {
        let Some(under) = self.under.as_mut() else {
            return;
        };
        std::sync::Arc::make_mut(under).stale.insert(id);
        let pairs: Vec<Pair> = self.enabled.iter().cloned().collect();
        for (label, field) in pairs {
            self.remove_node(&label, &field, id);
        }
    }

    /// Whether `(label, field)` is indexed, by either kind.
    pub fn is_enabled(&self, label: &str, field: &str) -> bool {
        self.enabled.contains(&pair(label, field))
    }

    /// Whether `(label, field)` carries an equality declaration (`EnableIndex`).
    pub fn is_equality_declared(&self, label: &str, field: &str) -> bool {
        self.equality.contains(&pair(label, field))
    }

    /// The kind of `(label, field)`, `None` when it is not indexed.
    pub fn kind_of(&self, label: &str, field: &str) -> Option<IndexKind> {
        self.kind.get(&pair(label, field)).copied()
    }

    /// Whether any declared index covers `field` (any label). Cheap guard for
    /// the maintenance hooks, matching `FulltextIndex::field_indexed`.
    pub fn field_indexed(&self, field: &str) -> bool {
        self.enabled.iter().any(|(_, f)| f == field)
    }

    /// Whether any field is declared for `label`.
    pub fn has_label(&self, label: &str) -> bool {
        self.enabled.iter().any(|(l, _)| l == label)
    }

    /// Iterate every maintained `(label, field)` pair, of either kind.
    pub fn enabled_pairs(&self) -> impl Iterator<Item = &(String, String)> {
        self.enabled.iter()
    }

    /// Iterate the pairs declared by `EnableIndex`, sorted. These, not
    /// [`enabled_pairs`](Self::enabled_pairs), are what a truncating snapshot
    /// re-emits as `EnableIndex` records.
    pub fn equality_pairs(&self) -> impl Iterator<Item = &(String, String)> {
        self.equality.iter()
    }

    /// Iterate the declared range indexes as `(name, (label, field))`, by name.
    pub fn range_indexes(&self) -> impl Iterator<Item = (&String, &(String, String))> {
        self.range_names.iter()
    }

    /// The pair the range index `name` covers.
    pub fn range_index(&self, name: &str) -> Option<&(String, String)> {
        self.range_names.get(name)
    }

    /// The name of the range index on `(label, field)`, if one is declared.
    pub fn range_index_on(&self, label: &str, field: &str) -> Option<&str> {
        self.range_names
            .iter()
            .find(|(_, (l, f))| l == label && f == field)
            .map(|(name, _)| name.as_str())
    }

    /// Whether this is a read-only layered index ([`Self::layered_over`]),
    /// whose range and prefix lookups see only the layer's own entries.
    pub fn is_layered(&self) -> bool {
        self.under.is_some()
    }

    /// The nodes with an entry under `(label, field)` — those whose value
    /// there has an index key — in ascending id order. On a layered index, the
    /// layer's own entries only.
    pub fn indexed_ids(&self, label: &str, field: &str) -> impl Iterator<Item = u32> + '_ {
        self.reverse
            .get(&pair(label, field))
            .into_iter()
            .flat_map(|by_id| by_id.keys().copied())
    }

    /// Whether any range index is declared.
    pub fn has_range_indexes(&self) -> bool {
        !self.range_names.is_empty()
    }

    /// Declare `(label, field)` as an equality index. Returns `true` if the
    /// equality declaration is new. A pair a range index already maintains
    /// keeps its postings and its `Range` kind.
    pub fn enable(&mut self, label: &str, field: &str) -> bool {
        let p = pair(label, field);
        self.enabled.insert(p.clone());
        self.kind.entry(p.clone()).or_insert(IndexKind::Equality);
        self.equality.insert(p)
    }

    /// Withdraw the equality declaration on `(label, field)`. The pair stops
    /// being indexed, and its entries are dropped, unless a range index still
    /// covers it. Returns `true` if it had been declared.
    pub fn disable(&mut self, label: &str, field: &str) -> bool {
        let p = pair(label, field);
        let was = self.equality.remove(&p);
        if self.kind.get(&p) != Some(&IndexKind::Range) {
            self.drop_pair(&p);
        }
        was
    }

    /// Declare the range index `name` on `(label, field)`: the pair becomes
    /// `Range` and is maintained from now on. The caller backfills a pair that
    /// was not maintained before; [`rebuild_all`](Self::rebuild_all) does it at
    /// open.
    pub fn enable_range(&mut self, name: &str, label: &str, field: &str) {
        let p = pair(label, field);
        self.enabled.insert(p.clone());
        self.kind.insert(p.clone(), IndexKind::Range);
        self.range_names.insert(name.to_string(), p);
    }

    /// Withdraw the range index `name`, returning the pair it covered. The
    /// pair falls back to `Equality` if `EnableIndex` also declared it, and is
    /// otherwise no longer indexed. `None` when no range index has that name.
    pub fn disable_range(&mut self, name: &str) -> Option<(String, String)> {
        let p = self.range_names.remove(name)?;
        if self.range_names.values().any(|other| *other == p) {
            return Some(p);
        }
        if self.equality.contains(&p) {
            self.kind.insert(p.clone(), IndexKind::Equality);
        } else {
            self.drop_pair(&p);
        }
        Some(p)
    }

    fn drop_pair(&mut self, p: &Pair) {
        self.enabled.remove(p);
        self.kind.remove(p);
        self.forward.remove(p);
        self.reverse.remove(p);
        self.offsets.remove(p);
    }

    /// Upsert node `id`'s value for `(label, field)`. Removes any prior entry
    /// first, then indexes `value` if it has an index key. A no-op when the
    /// pair is not declared.
    pub fn set(&mut self, label: &str, field: &str, id: u32, value: &Value) {
        let offset = match value {
            Value::DateTime { offset_seconds, .. } => *offset_seconds,
            _ => 0,
        };
        self.put(label, field, id, ValueKey::from_value(value), offset);
    }

    /// Move node `id`'s entry for `(label, field)` to `new`, or drop it when
    /// `new` is `None`. A no-op when the pair is not declared. A `DateTime`
    /// key given here carries no offset and is indexed as UTC; [`set`](Self::set)
    /// keeps the value's own.
    ///
    /// `old` is the key the caller believes it replaces. The reverse map is
    /// authoritative — the node's current entry is removed whatever `old`
    /// says — so `old` is checked only in debug builds, and `None` means
    /// "not known".
    pub fn update(
        &mut self,
        label: &str,
        field: &str,
        id: u32,
        old: Option<ValueKey>,
        new: Option<ValueKey>,
    ) {
        let current = self.put(label, field, id, new, 0);
        debug_assert!(
            old.is_none() || old == current,
            "update of ({label}, {field}, {id}) named {old:?}; the index held {current:?}"
        );
    }

    /// Replace node `id`'s entry for a declared pair, returning the key it
    /// held.
    fn put(
        &mut self,
        label: &str,
        field: &str,
        id: u32,
        new: Option<ValueKey>,
        offset: i32,
    ) -> Option<ValueKey> {
        if !self.is_enabled(label, field) {
            return None;
        }
        let p = pair(label, field);
        let current = self.remove_entry(&p, id);
        if let Some(vk) = new {
            if offset != 0 && matches!(vk, ValueKey::DateTimeMicrosUtc(_)) {
                self.offsets
                    .entry(p.clone())
                    .or_default()
                    .insert(id, offset);
            }
            let postings = self.forward.entry(p.clone()).or_default();
            match postings.get_mut(&vk) {
                Some(ids) => {
                    ids.insert(id);
                }
                None => {
                    postings.insert(vk.clone(), single(id));
                }
            }
            self.reverse.entry(p).or_default().insert(id, vk);
        }
        current
    }

    /// Remove every entry for node `id` across all declared pairs. Used when a
    /// node is deleted (mirrors [`crate::fulltext::FulltextIndex::remove_node`]).
    pub fn remove_node_all(&mut self, id: u32) {
        for (p, by_id) in self.reverse.iter_mut() {
            if let Some(old) = by_id.remove(&id) {
                remove_posting(self.forward.get_mut(p), &old, id);
                if let Some(offsets) = self.offsets.get_mut(p) {
                    offsets.remove(&id);
                }
            }
        }
    }

    /// Remove node `id`'s entry for `(label, field)` if present. Touches
    /// nothing for a pair that is not declared.
    pub fn remove_node(&mut self, label: &str, field: &str, id: u32) {
        self.remove_entry(&pair(label, field), id);
    }

    fn remove_entry(&mut self, p: &Pair, id: u32) -> Option<ValueKey> {
        let old = self.reverse.get_mut(p)?.remove(&id)?;
        remove_posting(self.forward.get_mut(p), &old, id);
        if let Some(offsets) = self.offsets.get_mut(p) {
            offsets.remove(&id);
        }
        Some(old)
    }

    /// Every indexed value of `(label, field)` and the node ids holding it, or
    /// `None` when nothing is indexed under the pair. Lets a caller probing
    /// many values look the pair up once instead of once per value.
    pub fn postings(&self, label: &str, field: &str) -> Option<&CowMap<ValueKey, IdSet>> {
        self.forward.get(&pair(label, field))
    }

    /// Whether [`lookup`](Self::lookup) can answer `= value`: it holds only
    /// values with an index key, so a `List`, `Map` or `Duration` operand —
    /// which a scan can find equal to a stored one — must be answered by a scan.
    pub fn answers_eq(value: &Value) -> bool {
        ValueKey::from_value(value).is_some()
    }

    /// Node ids of `label` whose `field` equals `value` as a scan's
    /// `values_equal` judges it, ascending: an `Int` finds the equal `Float`s
    /// and a `Float` the equal `Int`s, `0.0` and `-0.0` are equal, `NaN`
    /// equals nothing, and a `DateTime` ignores its offset. Empty when the pair
    /// is not declared or nothing matches, and for an operand
    /// [`answers_eq`](Self::answers_eq) refuses.
    pub fn lookup(&self, label: &str, field: &str, value: &Value) -> Vec<u32> {
        let own = self.own_lookup(label, field, value);
        let Some(under) = &self.under else {
            return own;
        };
        let mut ids: Vec<u32> = under
            .base
            .lookup(label, field, value)
            .into_iter()
            .filter(|id| !under.stale.contains(id))
            .chain(own)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// [`Self::lookup`] over this index's own postings only.
    fn own_lookup(&self, label: &str, field: &str, value: &Value) -> Vec<u32> {
        let Some(postings) = self.postings(label, field) else {
            return Vec::new();
        };
        let mut out: Vec<u32> = Vec::new();
        let mut take = |key: &ValueKey| {
            if let Some(ids) = postings.get(key) {
                out.extend(ids.iter());
            }
        };
        match value {
            Value::Int(i) => {
                take(&ValueKey::Int(*i));
                for f in with_other_zero(*i as f64) {
                    take(&ValueKey::FloatBits(f.to_bits()));
                }
            }
            Value::Float(f) if f.is_nan() => {}
            Value::Float(f) => {
                for g in with_other_zero(*f) {
                    take(&ValueKey::FloatBits(g.to_bits()));
                }
                // The `Int`s whose conversion is `f`: one below 2^53, a run of
                // neighbours above it, all between `f`'s float neighbours.
                if f.is_finite() && f.fract() == 0.0 {
                    let (lo, hi) = (f.next_down() as i64, f.next_up() as i64);
                    for (key, ids) in postings.range(ValueKey::Int(lo)..=ValueKey::Int(hi)) {
                        if matches!(key, ValueKey::Int(j) if *j as f64 == *f) {
                            out.extend(ids.iter());
                        }
                    }
                }
            }
            other => {
                if let Some(key) = ValueKey::from_value(other) {
                    take(&key);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The nodes of `label` whose `field` has exactly index key `key`, `None`
    /// when none does. A raw key lookup: [`lookup`](Self::lookup) is the one
    /// that answers as a scan does.
    pub fn lookup_eq(&self, label: &str, field: &str, key: &ValueKey) -> Option<&IdSet> {
        self.postings(label, field)?.get(key)
    }

    /// The nodes of `label` whose `field` lies between `lo` and `hi`, each
    /// bound given as `(value, inclusive)` and `None` for unbounded: exactly
    /// the nodes a scan's `eval_cmp` admits for `field >= lo` (or `>`) and
    /// `field <= hi` (or `<`).
    ///
    /// So, as in the scan: a bound orders only with its own class — numbers
    /// (`Int` and `Float` together, compared as `f64` by total order: `-0.0`
    /// sorts below `0.0`, `NaN` above `+inf` and `-NaN` below `-inf`), `Str`,
    /// `Date`, `LocalDateTime`, `DateTime` (by instant, then by offset), or
    /// `Binary`; a `Bool`, `List`, `Map` or `Duration` bound, or two bounds of
    /// different classes, match nothing; an empty or inverted range matches
    /// nothing. With both bounds `None`, every indexed node is returned.
    ///
    /// Ids come in value order (ties by id). The pair need not be a `Range`
    /// kind: the postings are the same, and which predicates to send here is
    /// the planner's decision.
    pub fn lookup_range(
        &self,
        label: &str,
        field: &str,
        lo: Option<(&Value, bool)>,
        hi: Option<(&Value, bool)>,
    ) -> impl Iterator<Item = u32> + '_ {
        let Some(postings) = self.postings(label, field) else {
            return empty();
        };
        let class = match (lo.map(|b| order_class(b.0)), hi.map(|b| order_class(b.0))) {
            (None, None) => return ids_of(postings.iter()),
            (Some(None), _) | (_, Some(None)) => return empty(),
            (Some(Some(a)), Some(Some(b))) if a != b => return empty(),
            (Some(Some(c)), _) | (None, Some(Some(c))) => c,
        };
        match class {
            OrderClass::Numeric => numeric_range(postings, lo, hi),
            OrderClass::DateTime => {
                datetime_range(postings, self.offsets.get(&pair(label, field)), lo, hi)
            }
            OrderClass::Str | OrderClass::Date | OrderClass::LocalDateTime | OrderClass::Binary => {
                key_range(postings, lo, hi)
            }
        }
    }

    /// The nodes of `label` whose `field` is a string starting with `prefix`,
    /// in key order. Only `Str` keys can match.
    pub fn lookup_prefix(
        &self,
        label: &str,
        field: &str,
        prefix: &str,
    ) -> impl Iterator<Item = u32> + '_ {
        let Some(postings) = self.postings(label, field) else {
            return empty();
        };
        let prefix = prefix.to_string();
        // Strings order by their UTF-8 bytes, so every string that starts with
        // `prefix` sits in one run beginning at `prefix` itself; the run ends at
        // the first key that does not start with it.
        ids_of(
            postings
                .range(ValueKey::Str(prefix.clone())..)
                .take_while(move |(k, _)| matches!(k, ValueKey::Str(s) if s.starts_with(&prefix))),
        )
    }

    /// Rebuild all postings for the declared pairs from live column data.
    /// Called at open end to correct any drift from per-record replay; a no-op
    /// when nothing is declared. Mirrors [`crate::fulltext::FulltextIndex::rebuild_all`].
    ///
    /// One pass per pair over the label's nodes; the postings and the reverse
    /// map are then each built from sorted input in one go rather than by a
    /// search-and-insert per node. At open this is the whole cost an index
    /// adds, and the first write pays nothing extra.
    pub fn rebuild_all(
        &mut self,
        ids: &IdMap,
        labels: &[u32],
        syms: &Interner,
        props: ColumnsView<'_>,
    ) {
        if self.enabled.is_empty() {
            return;
        }
        let n = ids.len().min(labels.len());
        let pairs: Vec<Pair> = self.enabled.iter().cloned().collect();
        for p in pairs {
            // Fixed-width keys are sorted as 16-byte integer triples rather
            // than as 40-byte `ValueKey`s: at a million nodes the sort is most
            // of the build, and its cost is moving the elements.
            let mut fixed: Vec<(u8, u64, u32)> = Vec::new();
            let mut variable: Vec<(ValueKey, u32)> = Vec::new();
            let mut offsets: Vec<(u32, i32)> = Vec::new();
            // A label no node carries has no symbol, and no entries.
            if let Some(sym) = syms.get(&p.0) {
                for (id, &node_sym) in labels.iter().enumerate().take(n) {
                    if node_sym != sym {
                        continue;
                    }
                    let id = id as u32;
                    let Some(vr) = props.get(id, &p.1) else {
                        continue;
                    };
                    let value = vr.as_value();
                    if let Value::DateTime { offset_seconds, .. } = value {
                        if *offset_seconds != 0 {
                            offsets.push((id, *offset_seconds));
                        }
                    }
                    let key = ValueKey::from_value(value);
                    match key.as_ref().and_then(fixed_width) {
                        Some((variant, ordinal)) => fixed.push((variant, ordinal, id)),
                        None => variable.extend(key.map(|k| (k, id))),
                    }
                }
            }
            // Both lists are in id order here: two sorted runs, which
            // `collect` merges and builds in bulk.
            let mut by_id: Vec<(u32, ValueKey)> = fixed
                .iter()
                .map(|&(variant, ordinal, id)| (id, from_fixed_width(variant, ordinal)))
                .chain(variable.iter().map(|(key, id)| (*id, key.clone())))
                .collect();
            // Two runs, each in id order: the stable sort merges them.
            by_id.sort_by_key(|(id, _)| *id);
            let by_id = CowMap::from_sorted(by_id);
            // By key; the ids of one key are gathered into a set, so their
            // order here does not matter.
            fixed.sort_unstable();
            variable.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let mut runs: Vec<(ValueKey, IdSet)> = Vec::new();
            let mut last: Option<(u8, u64)> = None;
            for (variant, ordinal, id) in fixed {
                match runs.last_mut() {
                    Some((_, ids)) if last == Some((variant, ordinal)) => {
                        ids.insert(id);
                    }
                    _ => {
                        runs.push((from_fixed_width(variant, ordinal), single(id)));
                        last = Some((variant, ordinal));
                    }
                }
            }
            let mut variable_runs: Vec<(ValueKey, IdSet)> = Vec::new();
            for (key, id) in variable {
                match variable_runs.last_mut() {
                    Some((prev, ids)) if *prev == key => {
                        ids.insert(id);
                    }
                    _ => variable_runs.push((key, single(id))),
                }
            }
            // Each list is in key order, but `Str` keys sort between the
            // fixed-width variants: the stable sort merges the two runs.
            runs.extend(variable_runs);
            runs.sort_by(|a, b| a.0.cmp(&b.0));
            let by_value: Postings = CowMap::from_sorted(runs);
            if by_value.is_empty() {
                self.forward.remove(&p);
                self.reverse.remove(&p);
            } else {
                self.forward.insert(p.clone(), by_value);
                self.reverse.insert(p.clone(), by_id);
            }
            if offsets.is_empty() {
                self.offsets.remove(&p);
            } else {
                self.offsets.insert(p, CowMap::from_sorted(offsets));
            }
        }
    }
}

/// A fixed-width key as `(variant, ordinal)`: equal keys map to equal pairs,
/// and the pairs order as the keys' own `Ord` does (variants in declaration
/// order, then each value as an order-preserving unsigned integer). `None` for
/// the variable-width `Str` and `BinaryBytes`.
fn fixed_width(k: &ValueKey) -> Option<(u8, u64)> {
    // Flipping the sign bit maps signed order onto unsigned order.
    const FLIP: u64 = 1 << 63;
    Some(match *k {
        ValueKey::Int(i) => (0, i as u64 ^ FLIP),
        ValueKey::FloatBits(bits) => (1, bits),
        ValueKey::Bool(b) => (3, b as u64),
        ValueKey::DateDays(d) => (4, i64::from(d) as u64 ^ FLIP),
        ValueKey::LocalDateTimeMicros(us) => (5, us as u64 ^ FLIP),
        ValueKey::DateTimeMicrosUtc(us) => (6, us as u64 ^ FLIP),
        ValueKey::Str(_) | ValueKey::BinaryBytes(_) => return None,
    })
}

/// The key [`fixed_width`] mapped to `(variant, ordinal)`.
fn from_fixed_width(variant: u8, ordinal: u64) -> ValueKey {
    const FLIP: u64 = 1 << 63;
    let signed = (ordinal ^ FLIP) as i64;
    match variant {
        0 => ValueKey::Int(signed),
        1 => ValueKey::FloatBits(ordinal),
        3 => ValueKey::Bool(ordinal != 0),
        4 => ValueKey::DateDays(signed as i32),
        5 => ValueKey::LocalDateTimeMicros(signed),
        6 => ValueKey::DateTimeMicrosUtc(signed),
        _ => unreachable!("fixed_width never yields variant {variant}"),
    }
}

/// A set holding `id` alone.
fn single(id: u32) -> IdSet {
    let mut ids = IdSet::new();
    ids.insert(id);
    ids
}

/// Drop `id` from the `old` bucket of one pair's postings, and the bucket
/// with it once it is empty.
fn remove_posting(by_value: Option<&mut Postings>, old: &ValueKey, id: u32) {
    if let Some(by_value) = by_value {
        if let Some(ids) = by_value.get_mut(old) {
            ids.remove(id);
            if ids.is_empty() {
                by_value.remove(old);
            }
        }
    }
}

/// `x`, and the other zero too when `x` is one: `0.0 == -0.0`.
fn with_other_zero(x: f64) -> Vec<f64> {
    if x == 0.0 {
        vec![0.0, -0.0]
    } else {
        vec![x]
    }
}

/// The classes a scan's `<` orders within (`core-query`'s `is_orderable_pair`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderClass {
    Numeric,
    Str,
    Date,
    LocalDateTime,
    DateTime,
    Binary,
}

fn order_class(v: &Value) -> Option<OrderClass> {
    match v {
        Value::Int(_) | Value::Float(_) => Some(OrderClass::Numeric),
        Value::Str(_) => Some(OrderClass::Str),
        Value::Date(_) => Some(OrderClass::Date),
        Value::LocalDateTime(_) => Some(OrderClass::LocalDateTime),
        Value::DateTime { .. } => Some(OrderClass::DateTime),
        Value::Binary(_) => Some(OrderClass::Binary),
        Value::Bool(_) | Value::List(_) | Value::Map(_) | Value::Duration { .. } | Value::Null => {
            None
        }
    }
}

type IdIter<'a> = Box<dyn Iterator<Item = u32> + 'a>;

fn empty<'a>() -> IdIter<'a> {
    Box::new(std::iter::empty())
}

fn ids_of<'a>(entries: impl Iterator<Item = (&'a ValueKey, &'a IdSet)> + 'a) -> IdIter<'a> {
    Box::new(entries.flat_map(|(_, ids)| ids.iter()))
}

fn same_variant(a: &ValueKey, b: &ValueKey) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// Whether `ord`, a stored value compared with a bound, satisfies the bound.
fn admits(ord: std::cmp::Ordering, lower: bool, inclusive: bool) -> bool {
    use std::cmp::Ordering::*;
    match (ord, lower) {
        (Equal, _) => inclusive,
        (Greater, true) | (Less, false) => true,
        _ => false,
    }
}

/// A range over a class whose key order is the scan's order: `Str`, `Date`,
/// `LocalDateTime` and `Binary`.
fn key_range<'a>(
    postings: &'a Postings,
    lo: Option<(&Value, bool)>,
    hi: Option<(&Value, bool)>,
) -> IdIter<'a> {
    let key = |b: Option<(&Value, bool)>| {
        b.map(|(v, incl)| {
            (
                ValueKey::from_value(v).expect("an ordered class has keys"),
                incl,
            )
        })
    };
    let (lo, hi) = (key(lo), key(hi));
    if let (Some((l, li)), Some((h, hincl))) = (&lo, &hi) {
        if l > h || (l == h && !(*li && *hincl)) {
            return empty();
        }
    }
    let variant = lo.as_ref().or(hi.as_ref()).expect("a bound").0.clone();
    let start = match lo {
        Some((k, true)) => Bound::Included(k),
        Some((k, false)) => Bound::Excluded(k),
        None => Bound::Included(variant_min(&variant)),
    };
    let end = match hi {
        Some((k, true)) => Bound::Included(k),
        Some((k, false)) => Bound::Excluded(k),
        None => Bound::Unbounded,
    };
    ids_of(
        postings
            .range((start, end))
            .take_while(move |(k, _)| same_variant(k, &variant)),
    )
}

/// The smallest key of `k`'s variant.
fn variant_min(k: &ValueKey) -> ValueKey {
    match k {
        ValueKey::Int(_) => ValueKey::Int(i64::MIN),
        ValueKey::FloatBits(_) => ValueKey::FloatBits(0),
        ValueKey::Str(_) => ValueKey::Str(String::new()),
        ValueKey::Bool(_) => ValueKey::Bool(false),
        ValueKey::DateDays(_) => ValueKey::DateDays(i32::MIN),
        ValueKey::LocalDateTimeMicros(_) => ValueKey::LocalDateTimeMicros(i64::MIN),
        ValueKey::DateTimeMicrosUtc(_) => ValueKey::DateTimeMicrosUtc(i64::MIN),
        ValueKey::BinaryBytes(_) => ValueKey::BinaryBytes(Vec::new()),
    }
}

/// A numeric bound as the scan compares it: an `Int` converted to `f64`.
fn as_number(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => unreachable!("a numeric bound"),
    }
}

/// `f64` bits remapped so that unsigned order is `f64::total_cmp` order.
fn total_order(bits: u64) -> u64 {
    const SIGN: u64 = 1 << 63;
    if bits & SIGN != 0 {
        !bits
    } else {
        bits | SIGN
    }
}

/// A numeric range: `Int` and `Float` keys together, compared as the scan
/// compares them — the `Int` converted to `f64`, then `f64::total_cmp`.
///
/// Each side is narrowed to a conservative key run and then filtered by that
/// exact comparison. `Int`s convert monotonically, so theirs is one run
/// between the integers just outside the bounds' float neighbours. `Float`
/// keys are raw bits, whose order is total order for a clear sign bit and its
/// reverse for a set one, so theirs is up to two runs.
fn numeric_range<'a>(
    postings: &'a Postings,
    lo: Option<(&Value, bool)>,
    hi: Option<(&Value, bool)>,
) -> IdIter<'a> {
    const SIGN: u64 = 1 << 63;
    let lo = lo.map(|(v, incl)| (as_number(v), incl));
    let hi = hi.map(|(v, incl)| (as_number(v), incl));
    let within = move |x: f64| {
        lo.is_none_or(|(b, incl)| admits(x.total_cmp(&b), true, incl))
            && hi.is_none_or(|(b, incl)| admits(x.total_cmp(&b), false, incl))
    };
    let mut hits: Vec<(f64, u32)> = Vec::new();

    // `Int`s: `as i64` saturates, and a NaN bound admits every integer or none.
    let int_lo = match lo {
        None => Some(i64::MIN),
        Some((b, _)) if b.is_nan() => b.is_sign_negative().then_some(i64::MIN),
        Some((b, _)) => Some(b.next_down().floor() as i64),
    };
    let int_hi = match hi {
        None => Some(i64::MAX),
        Some((b, _)) if b.is_nan() => b.is_sign_positive().then_some(i64::MAX),
        Some((b, _)) => Some(b.next_up().ceil() as i64),
    };
    if let (Some(a), Some(b)) = (int_lo, int_hi) {
        if a <= b {
            for (key, ids) in postings.range(ValueKey::Int(a)..=ValueKey::Int(b)) {
                if let ValueKey::Int(j) = key {
                    let x = *j as f64;
                    if within(x) {
                        hits.extend(ids.iter().map(|id| (x, id)));
                    }
                }
            }
        }
    }

    // `Float`s, in total-order space.
    let t_lo = lo.map_or(0, |(b, _)| total_order(b.to_bits()));
    let t_hi = hi.map_or(u64::MAX, |(b, _)| total_order(b.to_bits()));
    if t_lo <= t_hi {
        let mut runs: Vec<(u64, u64)> = Vec::new();
        // Clear sign bit: raw bits are total order with the sign bit removed.
        if t_hi >= SIGN {
            runs.push((t_lo.max(SIGN) ^ SIGN, t_hi ^ SIGN));
        }
        // Set sign bit: raw bits are the complement, so the run reverses.
        if t_lo < SIGN {
            runs.push((!t_hi.min(SIGN - 1), !t_lo));
        }
        for (a, b) in runs {
            for (key, ids) in postings.range(ValueKey::FloatBits(a)..=ValueKey::FloatBits(b)) {
                if let ValueKey::FloatBits(bits) = key {
                    let x = f64::from_bits(*bits);
                    if within(x) {
                        hits.extend(ids.iter().map(|id| (x, id)));
                    }
                }
            }
        }
    }
    hits.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    Box::new(hits.into_iter().map(|(_, id)| id))
}

/// A `DateTime` range. The scan orders by `(instant, offset)`, the key holds
/// the instant, and `offsets` the non-zero offsets: the instants between the
/// bounds' are a key run, filtered by the exact pair comparison.
fn datetime_range<'a>(
    postings: &'a Postings,
    offsets: Option<&'a CowMap<u32, i32>>,
    lo: Option<(&Value, bool)>,
    hi: Option<(&Value, bool)>,
) -> IdIter<'a> {
    let pair_of = |b: Option<(&Value, bool)>| {
        b.map(|(v, incl)| match v {
            Value::DateTime {
                micros_utc,
                offset_seconds,
            } => ((*micros_utc, *offset_seconds), incl),
            _ => unreachable!("a DateTime bound"),
        })
    };
    let (lo, hi) = (pair_of(lo), pair_of(hi));
    let start = lo.map_or(i64::MIN, |((us, _), _)| us);
    let end = hi.map_or(i64::MAX, |((us, _), _)| us);
    if start > end {
        return empty();
    }
    let range = ValueKey::DateTimeMicrosUtc(start)..=ValueKey::DateTimeMicrosUtc(end);
    Box::new(postings.range(range).flat_map(move |(key, ids)| {
        let ValueKey::DateTimeMicrosUtc(us) = *key else {
            unreachable!("a DateTime key run")
        };
        ids.iter().filter(move |id| {
            let offset = offsets.and_then(|o| o.get(id)).copied().unwrap_or(0);
            let stored = (us, offset);
            lo.is_none_or(|(b, incl)| admits(stored.cmp(&b), true, incl))
                && hi.is_none_or(|(b, incl)| admits(stored.cmp(&b), false, incl))
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_ids_are_the_nodes_whose_value_has_an_index_key() {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("r", "P", "v");
        ix.set("P", "v", 4, &Value::Int(1));
        ix.set("P", "v", 1, &Value::Int(1));
        ix.set("P", "v", 2, &Value::List(vec![Value::Int(1)]));
        ix.set("P", "v", 3, &Value::Str("a".into()));
        ix.remove_node("P", "v", 3);
        // Ascending; a list has no key; a removed entry is gone.
        assert_eq!(ix.indexed_ids("P", "v").collect::<Vec<_>>(), [1, 4]);
        assert_eq!(ix.indexed_ids("P", "w").count(), 0, "another field");
        assert!(!ix.is_layered());
        let layer = UnifiedIndex::layered_over(std::sync::Arc::new(ix), BTreeSet::new());
        assert!(layer.is_layered());
    }

    #[test]
    fn a_layered_index_answers_as_the_reindexed_base() {
        let mut base = UnifiedIndex::new();
        base.enable("P", "city");
        base.set("P", "city", 1, &Value::Str("a".into()));
        base.set("P", "city", 2, &Value::Str("a".into()));
        base.set("P", "city", 3, &Value::Str("b".into()));
        let base = std::sync::Arc::new(base);
        // Node 2 moved to "b", node 3 was deleted, node 4 is new in "a".
        let mut layer = UnifiedIndex::layered_over(std::sync::Arc::clone(&base), [2, 3, 4].into());
        layer.set("P", "city", 2, &Value::Str("b".into()));
        layer.set("P", "city", 4, &Value::Str("a".into()));
        assert!(layer.is_enabled("P", "city"));
        assert_eq!(layer.lookup("P", "city", &Value::Str("a".into())), [1, 4]);
        assert_eq!(layer.lookup("P", "city", &Value::Str("b".into())), [2]);
        assert_eq!(base.lookup("P", "city", &Value::Str("a".into())), [1, 2]);
        // A later commit moves node 2 again and node 1 for the first time.
        layer.restale(2);
        layer.set("P", "city", 2, &Value::Str("c".into()));
        layer.restale(1);
        assert_eq!(layer.lookup("P", "city", &Value::Str("a".into())), [4]);
        assert_eq!(
            layer.lookup("P", "city", &Value::Str("b".into())),
            Vec::<u32>::new()
        );
        assert_eq!(layer.lookup("P", "city", &Value::Str("c".into())), [2]);
    }

    /// Ledger row 126: the reader's fold holds a clone of the index while the
    /// writer goes on maintaining its own. The clone answers every lookup,
    /// range and prefix as it did when it was taken, and the writer's copy
    /// answers as an index built from its final values.
    #[test]
    fn a_clone_answers_as_it_did_while_the_original_is_written() {
        let mut live = UnifiedIndex::new();
        live.enable("P", "v");
        live.enable_range("by_v", "P", "w");
        for id in 0..5_000u32 {
            live.set("P", "v", id, &Value::Str(format!("s{}", id % 700)));
            live.set("P", "w", id, &iv(i64::from(id % 300)));
        }
        let frozen = live.clone();
        let answers = |ix: &UnifiedIndex| {
            (
                ix.lookup("P", "v", &Value::Str("s7".into())),
                ix.lookup_range("P", "w", Some((&iv(10), true)), Some((&iv(20), false)))
                    .collect::<Vec<_>>(),
                ix.lookup_prefix("P", "v", "s6").collect::<Vec<_>>(),
            )
        };
        let before = answers(&frozen);
        let mut want = UnifiedIndex::new();
        want.enable("P", "v");
        want.enable_range("by_v", "P", "w");
        for id in 0..6_000u32 {
            if id % 11 == 0 {
                live.remove_node_all(id);
                continue;
            }
            let v = Value::Str(format!("s{}", (id * 7) % 900));
            let w = iv(i64::from((id * 3) % 300));
            live.set("P", "v", id, &v);
            live.set("P", "w", id, &w);
            want.set("P", "v", id, &v);
            want.set("P", "w", id, &w);
        }
        assert_eq!(answers(&frozen), before);
        assert_eq!(answers(&live), answers(&want));
        assert_eq!(live.postings("P", "v"), want.postings("P", "v"));
        assert_eq!(live.postings("P", "w"), want.postings("P", "w"));
    }

    fn int(i: i64) -> ValueKey {
        ValueKey::Int(i)
    }

    fn float(f: f64) -> ValueKey {
        ValueKey::FloatBits(f.to_bits())
    }

    fn iv(i: i64) -> Value {
        Value::Int(i)
    }

    fn fv(f: f64) -> Value {
        Value::Float(f)
    }

    fn range(
        ix: &UnifiedIndex,
        lo: Option<(&Value, bool)>,
        hi: Option<(&Value, bool)>,
    ) -> Vec<u32> {
        ix.lookup_range("N", "v", lo, hi).collect()
    }

    #[test]
    fn test_range_lookup_int() {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("idx", "Score", "val");
        for (node, score) in [(0u32, 1i64), (1, 3), (2, 5), (3, 7), (4, 9)] {
            ix.update("Score", "val", node, None, Some(ValueKey::Int(score)));
        }
        let hits: Vec<u32> = ix
            .lookup_range(
                "Score",
                "val",
                Some((&Value::Int(3), true)),
                Some((&Value::Int(7), true)),
            )
            .collect();
        assert_eq!(hits, vec![1, 2, 3]);
    }

    #[test]
    fn test_range_lookup_starts_with() {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("idx", "P", "name");
        for (node, name) in [(0u32, "apple"), (1, "apricot"), (2, "banana")] {
            ix.update("P", "name", node, None, Some(ValueKey::Str(name.into())));
        }
        let hits: Vec<u32> = ix.lookup_prefix("P", "name", "ap").collect();
        assert_eq!(hits, vec![0, 1]);
    }

    /// `<`, `<=`, `>`, `>=`, a closed range, and an empty or inverted one.
    #[test]
    fn every_bound_shape_on_ints() {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("idx", "N", "v");
        for (node, v) in [(0u32, -5i64), (1, 0), (2, 3), (3, 3), (4, 8)] {
            ix.set("N", "v", node, &iv(v));
        }
        assert_eq!(range(&ix, None, Some((&iv(3), false))), vec![0, 1]);
        assert_eq!(range(&ix, None, Some((&iv(3), true))), vec![0, 1, 2, 3]);
        assert_eq!(range(&ix, Some((&iv(3), false)), None), vec![4]);
        assert_eq!(range(&ix, Some((&iv(3), true)), None), vec![2, 3, 4]);
        assert_eq!(
            range(&ix, Some((&iv(0), true)), Some((&iv(8), false))),
            vec![1, 2, 3]
        );
        assert_eq!(
            range(&ix, Some((&iv(3), true)), Some((&iv(3), true))),
            vec![2, 3]
        );
        // Empty and inverted ranges match nothing and do not panic.
        assert!(range(&ix, Some((&iv(3), false)), Some((&iv(3), true))).is_empty());
        assert!(range(&ix, Some((&iv(3), true)), Some((&iv(3), false))).is_empty());
        assert!(range(&ix, Some((&iv(8), true)), Some((&iv(0), true))).is_empty());
        assert_eq!(range(&ix, None, None), vec![0, 1, 2, 3, 4]);
    }

    /// The scan's classes: numbers together, every other ordered type alone,
    /// `Bool` never ordered, and bounds of two classes match nothing.
    #[test]
    fn a_range_matches_its_class_as_the_scan_does() {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("idx", "N", "v");
        ix.set("N", "v", 0, &iv(1));
        ix.set("N", "v", 1, &fv(2.0));
        ix.set("N", "v", 2, &Value::Str("3".into()));
        ix.set("N", "v", 3, &Value::Bool(true));
        ix.set("N", "v", 4, &iv(5));
        ix.set("N", "v", 5, &Value::Date(10));
        assert_eq!(range(&ix, Some((&iv(0), true)), None), vec![0, 1, 4]);
        assert_eq!(range(&ix, None, Some((&fv(1.5), true))), vec![0]);
        assert_eq!(
            range(&ix, Some((&iv(0), true)), Some((&fv(9.0), true))),
            vec![0, 1, 4]
        );
        assert_eq!(
            range(&ix, None, Some((&Value::Str("z".into()), true))),
            vec![2]
        );
        assert_eq!(range(&ix, Some((&Value::Date(0), true)), None), vec![5]);
        assert!(range(&ix, Some((&Value::Bool(false), false)), None).is_empty());
        assert!(range(
            &ix,
            Some((&iv(0), true)),
            Some((&Value::Str("z".into()), true))
        )
        .is_empty());
        assert!(range(&ix, Some((&Value::List(vec![]), true)), None).is_empty());
    }

    /// Numbers order by `f64::total_cmp`, as the scan's do.
    #[test]
    fn a_numeric_range_uses_total_order() {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("idx", "N", "v");
        let values = [
            (0u32, fv(-10.5)),
            (1, fv(-1.0)),
            (2, fv(-0.0)),
            (3, fv(0.0)),
            (4, fv(0.25)),
            (5, fv(7.0)),
            (6, fv(f64::INFINITY)),
            (7, fv(f64::NEG_INFINITY)),
            (8, fv(f64::NAN)),
            (9, iv(0)),
            (10, iv(7)),
            (11, fv(-f64::NAN)),
        ];
        for (node, v) in &values {
            ix.set("N", "v", *node, v);
        }
        assert_eq!(
            range(&ix, Some((&fv(-2.0), true)), Some((&fv(1.0), true))),
            vec![1, 2, 3, 9, 4]
        );
        assert_eq!(
            range(&ix, None, Some((&fv(0.0), false))),
            vec![11, 7, 0, 1, 2],
            "-0.0 sorts below 0.0, -NaN below everything"
        );
        assert_eq!(
            range(&ix, Some((&fv(0.0), true)), None),
            vec![3, 9, 4, 5, 10, 6, 8]
        );
        assert_eq!(
            range(&ix, Some((&fv(100.0), false)), None),
            vec![6, 8],
            "NaN is above +inf"
        );
        assert_eq!(
            range(&ix, Some((&iv(7), true)), Some((&iv(7), true))),
            vec![5, 10]
        );
        assert!(range(&ix, Some((&fv(5.0), true)), Some((&fv(1.0), true))).is_empty());
        assert_eq!(range(&ix, Some((&fv(f64::NAN), true)), None), vec![8]);
    }

    /// Equality is the scan's: numbers across `Int` and `Float`, both zeros,
    /// no `NaN`, and the `Int`s that round to a large `Float`.
    #[test]
    fn equality_is_numeric_as_the_scan_is() {
        let mut ix = UnifiedIndex::new();
        ix.enable("N", "v");
        ix.set("N", "v", 0, &iv(2));
        ix.set("N", "v", 1, &fv(2.0));
        ix.set("N", "v", 2, &fv(-0.0));
        ix.set("N", "v", 3, &iv(0));
        ix.set("N", "v", 4, &fv(f64::NAN));
        let big = 1i64 << 60;
        ix.set("N", "v", 5, &iv(big));
        ix.set("N", "v", 6, &iv(big + 1));
        ix.set("N", "v", 7, &fv(big as f64));
        ix.set("N", "v", 8, &fv(2.5));
        let eq = |v: &Value| ix.lookup("N", "v", v);
        assert_eq!(eq(&iv(2)), vec![0, 1]);
        assert_eq!(eq(&fv(2.0)), vec![0, 1]);
        assert_eq!(eq(&fv(0.0)), vec![2, 3]);
        assert_eq!(eq(&iv(0)), vec![2, 3]);
        assert!(eq(&fv(f64::NAN)).is_empty());
        assert_eq!(eq(&fv(big as f64)), vec![5, 6, 7]);
        assert_eq!(eq(&iv(big + 1)), vec![6, 7]);
        assert_eq!(eq(&fv(2.5)), vec![8]);
        assert!(!UnifiedIndex::answers_eq(&Value::List(vec![])));
        assert!(UnifiedIndex::answers_eq(&iv(1)));
    }

    /// The key keeps the instant only; the scan's `<` breaks a tie by offset.
    #[test]
    fn a_datetime_range_orders_a_tie_by_offset() {
        let dt = |us: i64, off: i32| Value::DateTime {
            micros_utc: us,
            offset_seconds: off,
        };
        let mut ix = UnifiedIndex::new();
        ix.enable_range("idx", "N", "v");
        ix.set("N", "v", 0, &dt(10, 0));
        ix.set("N", "v", 1, &dt(10, 3600));
        ix.set("N", "v", 2, &dt(10, -3600));
        ix.set("N", "v", 3, &dt(20, 0));
        assert_eq!(range(&ix, None, Some((&dt(10, 0), false))), vec![2]);
        assert_eq!(range(&ix, Some((&dt(10, 0), false)), None), vec![1, 3]);
        assert_eq!(
            range(&ix, Some((&dt(10, 0), true)), Some((&dt(10, 0), true))),
            vec![0]
        );
        assert_eq!(ix.lookup("N", "v", &dt(10, 7200)), vec![0, 1, 2]);
        // An update moves the offset with the value.
        ix.set("N", "v", 1, &dt(10, 0));
        assert_eq!(
            range(&ix, Some((&dt(10, 0), true)), Some((&dt(10, 0), true))),
            vec![0, 1]
        );
        ix.remove_node_all(2);
        assert_eq!(
            range(&ix, None, Some((&dt(10, 0), false))),
            Vec::<u32>::new()
        );
    }

    /// Minor 6: removing from a pair nobody declared leaves nothing behind.
    #[test]
    fn removing_from_an_undeclared_pair_allocates_nothing() {
        let mut ix = UnifiedIndex::new();
        ix.enable("N", "v");
        ix.remove_node("Other", "v", 3);
        ix.remove_node("N", "v", 3);
        assert!(ix.reverse.is_empty());
        assert!(ix.offsets.is_empty());
    }

    #[test]
    fn a_prefix_matches_strings_only_and_handles_the_edges() {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("idx", "N", "v");
        let strs = ["ap", "apple", "apz", "aq", "a", "\u{10FFFF}x", "\u{10FFFF}"];
        for (node, s) in strs.iter().enumerate() {
            ix.update(
                "N",
                "v",
                node as u32,
                None,
                Some(ValueKey::Str((*s).into())),
            );
        }
        ix.update("N", "v", 20, None, Some(ValueKey::Bool(true)));
        ix.update("N", "v", 21, None, Some(int(1)));
        let p = |s: &str| ix.lookup_prefix("N", "v", s).collect::<Vec<u32>>();
        assert_eq!(p("ap"), vec![0, 1, 2]);
        assert_eq!(p("\u{10FFFF}"), vec![6, 5], "no successor character exists");
        assert_eq!(
            p(""),
            vec![4, 0, 1, 2, 3, 6, 5],
            "every string, nothing else"
        );
        assert!(p("b").is_empty());
    }

    /// The rebuild sorts fixed-width keys by their `(variant, ordinal)` form,
    /// which must round-trip and order exactly as the keys do.
    #[test]
    fn the_fixed_width_form_round_trips_and_keeps_key_order() {
        let mut keys = vec![
            int(i64::MIN),
            int(-1),
            int(0),
            int(i64::MAX),
            float(-3.5),
            float(-0.0),
            float(0.0),
            float(2.0),
            float(f64::NAN),
            ValueKey::Bool(false),
            ValueKey::Bool(true),
            ValueKey::DateDays(i32::MIN),
            ValueKey::DateDays(-1),
            ValueKey::DateDays(i32::MAX),
            ValueKey::LocalDateTimeMicros(i64::MIN),
            ValueKey::LocalDateTimeMicros(5),
            ValueKey::DateTimeMicrosUtc(-7),
            ValueKey::DateTimeMicrosUtc(i64::MAX),
        ];
        for k in &keys {
            let (variant, ordinal) = fixed_width(k).expect("fixed width");
            assert_eq!(&from_fixed_width(variant, ordinal), k);
        }
        let mut by_form = keys.clone();
        by_form.sort_by_key(|k| fixed_width(k).unwrap());
        keys.sort();
        assert_eq!(by_form, keys);
        assert!(fixed_width(&ValueKey::Str("a".into())).is_none());
        assert!(fixed_width(&ValueKey::BinaryBytes(vec![1])).is_none());
    }

    /// The rebuild gives the postings incremental maintenance gives, over a
    /// column holding every key type at once, duplicates included.
    #[test]
    fn rebuild_all_matches_incremental_maintenance() {
        use crate::columns::ColumnStore;
        use crate::types::Value;
        let values = [
            Value::Int(3),
            Value::Str("b".into()),
            Value::Int(-2),
            Value::Float(1.5),
            Value::Bool(true),
            Value::Int(3),
            Value::Str("a".into()),
            Value::Date(9),
            Value::Binary(vec![1, 2]),
            Value::List(vec![]),
            Value::Str("b".into()),
            Value::DateTime {
                micros_utc: 4,
                offset_seconds: 60,
            },
            Value::LocalDateTime(-4),
        ];
        let mut ids = IdMap::new();
        let mut syms = Interner::new();
        let n_sym = syms.intern("N");
        let other = syms.intern("Other");
        let mut cols = ColumnStore::new();
        let mut labels = Vec::new();
        let mut incremental = UnifiedIndex::new();
        incremental.enable_range("r", "N", "v");
        for (i, v) in values.iter().enumerate() {
            let id = ids.try_insert(&format!("k{i}")).unwrap();
            // Every fourth node is another label's, which the pair ignores.
            labels.push(if i % 4 == 3 { other } else { n_sym });
            cols.set(id, "v", v.clone());
            if i % 4 != 3 {
                incremental.set("N", "v", id, v);
            }
        }
        let mut rebuilt = UnifiedIndex::new();
        rebuilt.enable_range("r", "N", "v");
        rebuilt.rebuild_all(&ids, &labels, &syms, ColumnsView::owned(&cols));
        assert_eq!(rebuilt.postings("N", "v"), incremental.postings("N", "v"));
        assert_eq!(
            rebuilt.reverse, incremental.reverse,
            "the rebuild builds the reverse map"
        );
        assert_eq!(rebuilt.offsets, incremental.offsets);
        rebuilt.set("N", "v", 0, &Value::Int(100));
        incremental.set("N", "v", 0, &Value::Int(100));
        rebuilt.remove_node_all(1);
        incremental.remove_node_all(1);
        assert_eq!(rebuilt.postings("N", "v"), incremental.postings("N", "v"));
        assert_eq!(rebuilt.reverse, incremental.reverse);
        assert_eq!(rebuilt.offsets, incremental.offsets);
    }

    #[test]
    fn an_undeclared_pair_answers_nothing() {
        let ix = UnifiedIndex::new();
        assert_eq!(ix.lookup_range("N", "v", None, None).count(), 0);
        assert_eq!(ix.lookup_prefix("N", "v", "a").count(), 0);
        assert!(ix.lookup_eq("N", "v", &int(1)).is_none());
    }

    #[test]
    fn lookup_eq_follows_updates() {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("idx", "N", "v");
        ix.update("N", "v", 1, None, Some(int(4)));
        ix.update("N", "v", 2, None, Some(int(4)));
        assert_eq!(
            ix.lookup_eq("N", "v", &int(4))
                .map(|s| s.iter().collect::<Vec<_>>()),
            Some(vec![1, 2])
        );
        ix.update("N", "v", 1, Some(int(4)), Some(int(6)));
        ix.update("N", "v", 2, Some(int(4)), None);
        assert!(ix.lookup_eq("N", "v", &int(4)).is_none());
        assert_eq!(range(&ix, None, None), vec![1]);
    }

    /// Range and equality declarations on one pair are independent: dropping
    /// either leaves the other, and dropping both stops the maintenance.
    #[test]
    fn range_and_equality_declarations_compose() {
        let mut ix = UnifiedIndex::new();
        assert!(!ix.has_range_indexes());
        ix.enable("N", "v");
        ix.set("N", "v", 1, &Value::Int(3));
        assert_eq!(ix.kind_of("N", "v"), Some(IndexKind::Equality));
        ix.enable_range("r", "N", "v");
        assert!(ix.has_range_indexes());
        assert_eq!(ix.kind_of("N", "v"), Some(IndexKind::Range));
        assert_eq!(ix.range_index("r"), Some(&("N".into(), "v".into())));
        assert_eq!(ix.range_index_on("N", "v"), Some("r"));
        // Dropping the range index falls back to the equality index.
        assert_eq!(ix.disable_range("r"), Some(("N".into(), "v".into())));
        assert_eq!(ix.kind_of("N", "v"), Some(IndexKind::Equality));
        assert_eq!(ix.lookup("N", "v", &Value::Int(3)), vec![1]);
        assert!(!ix.has_range_indexes());
        assert_eq!(ix.disable_range("r"), None);
        // A range-only pair: withdrawing an equality declaration it never had
        // keeps it; dropping the range index removes it and its entries.
        ix.disable("N", "v");
        ix.enable_range("r2", "M", "w");
        ix.set("M", "w", 9, &Value::Int(1));
        assert!(!ix.disable("M", "w"));
        assert_eq!(ix.lookup("M", "w", &Value::Int(1)), vec![9]);
        assert!(!ix.is_equality_declared("M", "w"));
        ix.disable_range("r2");
        assert!(!ix.is_enabled("M", "w"));
        assert_eq!(ix.kind_of("M", "w"), None);
        assert!(ix.postings("M", "w").is_none());
        assert_eq!(ix.enabled_pairs().count(), 0);
        // Equality over a range pair: dropping the equality keeps the range.
        ix.enable_range("r3", "M", "w");
        ix.enable("M", "w");
        ix.set("M", "w", 9, &Value::Int(1));
        assert!(ix.disable("M", "w"));
        assert_eq!(ix.kind_of("M", "w"), Some(IndexKind::Range));
        assert_eq!(ix.lookup("M", "w", &Value::Int(1)), vec![9]);
        assert_eq!(ix.equality_pairs().count(), 0);
    }
}
