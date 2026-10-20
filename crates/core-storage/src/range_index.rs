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
//! Nothing here is persisted: declarations replay from the WAL and the
//! postings rebuild at open ([`UnifiedIndex::rebuild_all`]).

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
type Postings = BTreeMap<ValueKey, BTreeSet<u32>>;

/// Equality and range index over declared `(label, field)` pairs.
#[derive(Debug, Clone, Default)]
pub struct UnifiedIndex {
    /// Every maintained pair, whatever it was declared as. A pair not present
    /// here is not indexed.
    enabled: BTreeSet<Pair>,
    /// `(label, field)` → value → set of node ids holding that value.
    forward: BTreeMap<Pair, Postings>,
    /// `(label, field)` → id → the node's current indexed key, so an update or
    /// delete can drop the stale forward entry without the old value.
    ///
    /// Derived from `forward` on demand: [`rebuild_all`](Self::rebuild_all)
    /// leaves a pair's map out, and the first update or delete touching the
    /// pair builds it (see `reverse_of`), so an open does not pay for maps no
    /// write may ever need. A pair absent here is one whose map is not built.
    reverse: BTreeMap<Pair, BTreeMap<u32, ValueKey>>,
    /// The kind of every maintained pair: `Range` while a range index is
    /// declared on it, else `Equality`.
    kind: BTreeMap<Pair, IndexKind>,
    /// Pairs declared by `EnableIndex`. Kept apart from the range declarations
    /// so that dropping a range index over a pair that also has an equality
    /// index leaves the equality index in place.
    equality: BTreeSet<Pair>,
    /// Range index name → the pair it covers.
    range_names: BTreeMap<String, Pair>,
}

fn pair(label: &str, field: &str) -> Pair {
    (label.to_string(), field.to_string())
}

impl UnifiedIndex {
    pub fn new() -> Self {
        Self::default()
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
    }

    /// Upsert node `id`'s value for `(label, field)`. Removes any prior entry
    /// first, then indexes `value` if it is a scalar. A no-op when the pair is
    /// not declared or `value` is a list/map.
    pub fn set(&mut self, label: &str, field: &str, id: u32, value: &Value) {
        self.update(label, field, id, None, ValueKey::from_value(value));
    }

    /// Move node `id`'s entry for `(label, field)` to `new`, or drop it when
    /// `new` is `None`. A no-op when the pair is not declared.
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
        if !self.is_enabled(label, field) {
            return;
        }
        let current = self.remove_node_key(label, field, id);
        debug_assert!(
            old.is_none() || old == current,
            "update of ({label}, {field}, {id}) named {old:?}; the index held {current:?}"
        );
        if let Some(vk) = new {
            let p = pair(label, field);
            self.forward
                .entry(p.clone())
                .or_default()
                .entry(vk.clone())
                .or_default()
                .insert(id);
            self.reverse_of(&p).insert(id, vk);
        }
    }

    /// `p`'s reverse map, derived from its postings first if it is not built.
    fn reverse_of(&mut self, p: &Pair) -> &mut BTreeMap<u32, ValueKey> {
        if !self.reverse.contains_key(p) {
            let by_id = self.forward.get(p).map(reverse_from).unwrap_or_default();
            self.reverse.insert(p.clone(), by_id);
        }
        self.reverse.get_mut(p).expect("inserted above")
    }

    /// Remove every entry for node `id` across all declared pairs. Used when a
    /// node is deleted (mirrors [`crate::fulltext::FulltextIndex::remove_node`]).
    pub fn remove_node_all(&mut self, id: u32) {
        let pairs: Vec<Pair> = self.forward.keys().cloned().collect();
        for p in pairs {
            if let Some(old) = self.reverse_of(&p).remove(&id) {
                remove_posting(self.forward.get_mut(&p), &old, id);
            }
        }
    }

    /// Remove node `id`'s entry for `(label, field)` if present.
    pub fn remove_node(&mut self, label: &str, field: &str, id: u32) {
        self.remove_node_key(label, field, id);
    }

    fn remove_node_key(&mut self, label: &str, field: &str, id: u32) -> Option<ValueKey> {
        let p = pair(label, field);
        let old = self.reverse_of(&p).remove(&id)?;
        remove_posting(self.forward.get_mut(&p), &old, id);
        Some(old)
    }

    /// Every indexed value of `(label, field)` and the node ids holding it, or
    /// `None` when nothing is indexed under the pair. Lets a caller probing
    /// many values look the pair up once instead of once per value.
    pub fn postings(&self, label: &str, field: &str) -> Option<&BTreeMap<ValueKey, BTreeSet<u32>>> {
        self.forward.get(&pair(label, field))
    }

    /// Node ids of `label` whose `field` equals `value`, ascending. Empty when
    /// the pair is not declared, `value` is non-scalar, or nothing matches.
    pub fn lookup(&self, label: &str, field: &str, value: &Value) -> Vec<u32> {
        let Some(vk) = ValueKey::from_value(value) else {
            return Vec::new();
        };
        self.lookup_eq(label, field, &vk)
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The nodes of `label` whose `field` has index key `key`, `None` when
    /// none does.
    pub fn lookup_eq(&self, label: &str, field: &str, key: &ValueKey) -> Option<&BTreeSet<u32>> {
        self.postings(label, field)?.get(key)
    }

    /// The nodes of `label` whose `field` lies between `lo` and `hi`, each
    /// bound given as `(key, inclusive)` and `None` for unbounded.
    ///
    /// The match is by type: a bound's [`ValueKey`] variant names the type
    /// asked for, and only keys of that variant are returned — an `Int` range
    /// never yields a `Str`, a `Float` or a `Bool`, so a column holding mixed
    /// types answers with the matching ones only. Bounds of two different
    /// variants match nothing, as does an empty or inverted range. A `Float`
    /// range compares numerically (negative values included), and a `NaN`
    /// bound matches nothing. With both bounds `None`, every indexed node is
    /// returned.
    ///
    /// Ids come in key order, ascending within one key. The pair need not be
    /// a `Range` kind: the postings are the same, and which predicates to send
    /// here is the planner's decision.
    pub fn lookup_range(
        &self,
        label: &str,
        field: &str,
        lo: Option<(&ValueKey, bool)>,
        hi: Option<(&ValueKey, bool)>,
    ) -> impl Iterator<Item = u32> + '_ {
        let Some(postings) = self.postings(label, field) else {
            return empty();
        };
        let variant = match (lo, hi) {
            (None, None) => return ids_of(postings.iter()),
            (Some((l, _)), Some((h, _))) if !same_variant(l, h) => return empty(),
            (Some((k, _)), _) | (None, Some((k, _))) => k.clone(),
        };
        if let ValueKey::FloatBits(_) = variant {
            return float_range(postings, lo, hi);
        }
        if let (Some((l, li)), Some((h, hi_incl))) = (lo, hi) {
            if l > h || (l == h && !(li && hi_incl)) {
                return empty();
            }
        }
        let start = match lo {
            Some((k, true)) => Bound::Included(k.clone()),
            Some((k, false)) => Bound::Excluded(k.clone()),
            None => Bound::Included(variant_min(&variant)),
        };
        let end = match hi {
            Some((k, true)) => Bound::Included(k.clone()),
            Some((k, false)) => Bound::Excluded(k.clone()),
            None => Bound::Unbounded,
        };
        ids_of(
            postings
                .range((start, end))
                .take_while(move |(k, _)| same_variant(k, &variant)),
        )
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
    /// One pass per pair over the label's nodes, then the postings are built
    /// from sorted input in one go rather than by a search-and-insert per
    /// node, and the reverse map is left to the first write that needs it: at
    /// open this is the whole cost a range index adds.
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
            // A label no node carries has no symbol, and no entries.
            if let Some(sym) = syms.get(&p.0) {
                for (id, &node_sym) in labels.iter().enumerate().take(n) {
                    if node_sym != sym {
                        continue;
                    }
                    let id = id as u32;
                    let key = props
                        .get(id, &p.1)
                        .and_then(|vr| ValueKey::from_value(vr.as_value()));
                    match key.as_ref().and_then(fixed_width) {
                        Some((variant, ordinal)) => fixed.push((variant, ordinal, id)),
                        None => variable.extend(key.map(|k| (k, id))),
                    }
                }
            }
            // By key; the ids of one key are gathered into a set, so their
            // order here does not matter.
            fixed.sort_unstable();
            variable.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let mut runs: Vec<(ValueKey, BTreeSet<u32>)> = Vec::new();
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
            let mut variable_runs: Vec<(ValueKey, BTreeSet<u32>)> = Vec::new();
            for (key, id) in variable {
                match variable_runs.last_mut() {
                    Some((prev, ids)) if *prev == key => {
                        ids.insert(id);
                    }
                    _ => variable_runs.push((key, single(id))),
                }
            }
            // Two sorted runs of distinct keys: `collect` merges them and
            // builds the tree in bulk.
            let by_value: Postings = runs.into_iter().chain(variable_runs).collect();
            // The reverse map is derived from these postings when a write
            // first needs it.
            self.reverse.remove(&p);
            if by_value.is_empty() {
                self.forward.remove(&p);
            } else {
                self.forward.insert(p, by_value);
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
fn single(id: u32) -> BTreeSet<u32> {
    let mut ids = BTreeSet::new();
    ids.insert(id);
    ids
}

/// The id → key map of one pair's postings.
fn reverse_from(by_value: &Postings) -> BTreeMap<u32, ValueKey> {
    let mut by_id: Vec<(u32, ValueKey)> = by_value
        .iter()
        .flat_map(|(key, ids)| ids.iter().map(move |&id| (id, key.clone())))
        .collect();
    // An id holds one key per pair, so the ids are distinct.
    by_id.sort_unstable_by_key(|(id, _)| *id);
    by_id.into_iter().collect()
}

/// Drop `id` from the `old` bucket of one pair's postings, and the bucket
/// with it once it is empty.
fn remove_posting(by_value: Option<&mut Postings>, old: &ValueKey, id: u32) {
    if let Some(by_value) = by_value {
        if let Some(ids) = by_value.get_mut(old) {
            ids.remove(&id);
            if ids.is_empty() {
                by_value.remove(old);
            }
        }
    }
}

type IdIter<'a> = Box<dyn Iterator<Item = u32> + 'a>;

fn empty<'a>() -> IdIter<'a> {
    Box::new(std::iter::empty())
}

fn ids_of<'a>(entries: impl Iterator<Item = (&'a ValueKey, &'a BTreeSet<u32>)> + 'a) -> IdIter<'a> {
    Box::new(entries.flat_map(|(_, ids)| ids.iter().copied()))
}

fn same_variant(a: &ValueKey, b: &ValueKey) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// The smallest key of `k`'s variant. Not used for `FloatBits`, whose bit
/// order is not numeric order.
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

fn as_f64(k: &ValueKey) -> Option<f64> {
    match k {
        ValueKey::FloatBits(b) => Some(f64::from_bits(*b)),
        _ => None,
    }
}

/// A numeric `Float` range over keys stored as raw `f64` bits.
///
/// Bit order is numeric order for non-negative floats (sign bit clear) and the
/// reverse of it for negative ones (sign bit set), with `NaN`s past both
/// infinities. So the range is two bit runs: the negative run walked backwards,
/// then the non-negative run forwards. Each run is narrowed by the bounds and
/// then filtered by an exact numeric comparison, which settles the open ends,
/// `-0.0 == 0.0`, and keeps every `NaN` out.
fn float_range<'a>(
    postings: &'a Postings,
    lo: Option<(&ValueKey, bool)>,
    hi: Option<(&ValueKey, bool)>,
) -> IdIter<'a> {
    let lo = lo.and_then(|(k, incl)| as_f64(k).map(|v| (v, incl)));
    let hi = hi.and_then(|(k, incl)| as_f64(k).map(|v| (v, incl)));
    if lo.is_some_and(|(v, _)| v.is_nan()) || hi.is_some_and(|(v, _)| v.is_nan()) {
        return empty();
    }
    let within = move |x: f64| {
        let above = match lo {
            None => true,
            Some((v, true)) => x >= v,
            Some((v, false)) => x > v,
        };
        let below = match hi {
            None => true,
            Some((v, true)) => x <= v,
            Some((v, false)) => x < v,
        };
        !x.is_nan() && above && below
    };
    let bits = |v: f64| ValueKey::FloatBits(v.to_bits());
    // Negative run, bits from nearest zero to most negative.
    let neg = if lo.is_some_and(|(v, _)| v > 0.0) {
        None
    } else {
        let near = match hi {
            Some((v, _)) if v < 0.0 => v,
            _ => -0.0,
        };
        let far = match lo {
            Some((v, _)) => -v.abs(),
            None => f64::NEG_INFINITY,
        };
        (near.to_bits() <= far.to_bits()).then(|| (bits(near), bits(far)))
    };
    // Non-negative run, bits from zero to largest.
    let pos = if hi.is_some_and(|(v, _)| v < 0.0) {
        None
    } else {
        let low = match lo {
            Some((v, _)) if v > 0.0 => v,
            _ => 0.0,
        };
        let high = match hi {
            Some((v, _)) => v.abs(),
            None => f64::INFINITY,
        };
        (low.to_bits() <= high.to_bits()).then(|| (bits(low), bits(high)))
    };
    let neg_entries = neg
        .into_iter()
        .flat_map(move |(near, far)| postings.range(near..=far).rev());
    let pos_entries = pos
        .into_iter()
        .flat_map(move |(low, high)| postings.range(low..=high));
    ids_of(
        neg_entries
            .chain(pos_entries)
            .filter(move |(k, _)| as_f64(k).is_some_and(within)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(i: i64) -> ValueKey {
        ValueKey::Int(i)
    }

    fn float(f: f64) -> ValueKey {
        ValueKey::FloatBits(f.to_bits())
    }

    fn range(
        ix: &UnifiedIndex,
        lo: Option<(&ValueKey, bool)>,
        hi: Option<(&ValueKey, bool)>,
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
                Some((&ValueKey::Int(3), true)),
                Some((&ValueKey::Int(7), true)),
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
            ix.update("N", "v", node, None, Some(int(v)));
        }
        assert_eq!(range(&ix, None, Some((&int(3), false))), vec![0, 1]);
        assert_eq!(range(&ix, None, Some((&int(3), true))), vec![0, 1, 2, 3]);
        assert_eq!(range(&ix, Some((&int(3), false)), None), vec![4]);
        assert_eq!(range(&ix, Some((&int(3), true)), None), vec![2, 3, 4]);
        assert_eq!(
            range(&ix, Some((&int(0), true)), Some((&int(8), false))),
            vec![1, 2, 3]
        );
        assert_eq!(
            range(&ix, Some((&int(3), true)), Some((&int(3), true))),
            vec![2, 3]
        );
        // Empty and inverted ranges match nothing and do not panic.
        assert!(range(&ix, Some((&int(3), false)), Some((&int(3), true))).is_empty());
        assert!(range(&ix, Some((&int(3), true)), Some((&int(3), false))).is_empty());
        assert!(range(&ix, Some((&int(8), true)), Some((&int(0), true))).is_empty());
        assert_eq!(range(&ix, None, None), vec![0, 1, 2, 3, 4]);
    }

    /// A column holding several types: a bound's type selects keys of that
    /// type only, in both directions, and mixed-type bounds match nothing.
    #[test]
    fn a_range_matches_only_keys_of_its_own_type() {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("idx", "N", "v");
        ix.update("N", "v", 0, None, Some(int(1)));
        ix.update("N", "v", 1, None, Some(float(2.0)));
        ix.update("N", "v", 2, None, Some(ValueKey::Str("3".into())));
        ix.update("N", "v", 3, None, Some(ValueKey::Bool(true)));
        ix.update("N", "v", 4, None, Some(int(5)));
        ix.update("N", "v", 5, None, Some(ValueKey::DateDays(10)));
        assert_eq!(range(&ix, Some((&int(0), true)), None), vec![0, 4]);
        assert_eq!(range(&ix, None, Some((&int(100), true))), vec![0, 4]);
        assert_eq!(range(&ix, Some((&float(0.0), true)), None), vec![1]);
        assert_eq!(
            range(&ix, None, Some((&ValueKey::Str("z".into()), true))),
            vec![2]
        );
        assert_eq!(
            range(&ix, Some((&ValueKey::DateDays(0), true)), None),
            vec![5]
        );
        assert!(range(&ix, Some((&int(0), true)), Some((&float(9.0), true))).is_empty());
    }

    /// `Float` keys are raw bits, whose order is not numeric for negatives.
    #[test]
    fn a_float_range_is_numeric_across_zero() {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("idx", "N", "v");
        let values = [
            (0u32, -10.5),
            (1, -1.0),
            (2, -0.0),
            (3, 0.0),
            (4, 0.25),
            (5, 7.0),
            (6, f64::INFINITY),
            (7, f64::NEG_INFINITY),
            (8, f64::NAN),
        ];
        for (node, v) in values {
            ix.update("N", "v", node, None, Some(float(v)));
        }
        assert_eq!(
            range(&ix, Some((&float(-2.0), true)), Some((&float(1.0), true))),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            range(&ix, None, Some((&float(0.0), false))),
            vec![7, 0, 1],
            "ascending, and -0.0 is not below 0.0"
        );
        assert_eq!(range(&ix, Some((&float(0.0), false)), None), vec![4, 5, 6]);
        assert_eq!(
            range(&ix, Some((&float(-0.0), true)), Some((&float(0.0), true))),
            vec![2, 3]
        );
        assert_eq!(
            range(
                &ix,
                Some((&float(-11.0), true)),
                Some((&float(-1.0), false))
            ),
            vec![0]
        );
        assert!(range(&ix, Some((&float(5.0), true)), Some((&float(1.0), true))).is_empty());
        assert!(range(&ix, Some((&float(f64::NAN), true)), None).is_empty());
        let unbounded_lo: Vec<u32> = range(&ix, Some((&float(f64::NEG_INFINITY), true)), None);
        assert_eq!(
            unbounded_lo,
            vec![7, 0, 1, 2, 3, 4, 5, 6],
            "NaN never matches"
        );
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
        // The reverse map the rebuild left out is derived on the first write.
        rebuilt.set("N", "v", 0, &Value::Int(100));
        incremental.set("N", "v", 0, &Value::Int(100));
        rebuilt.remove_node_all(1);
        incremental.remove_node_all(1);
        assert_eq!(rebuilt.postings("N", "v"), incremental.postings("N", "v"));
        assert_eq!(rebuilt.reverse, incremental.reverse);
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
                .map(|s| s.iter().copied().collect::<Vec<_>>()),
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
