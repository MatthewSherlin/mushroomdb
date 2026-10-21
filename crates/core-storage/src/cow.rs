//! Copy-on-write containers whose clone shares their contents.
//!
//! The write path shares the store's structures with the reader's fold
//! (`Arc` clones), and the first commit after a fold copies, with
//! `Arc::make_mut`, every structure it writes to. With flat containers that
//! copy is the whole column store or the whole index: tens to hundreds of
//! milliseconds under the write lock at a million nodes (ledger row 126).
//!
//! These containers keep their contents in small pieces behind `Arc`s.
//! Cloning one shares the pieces, and a write copies only the pieces it lands
//! in, so the first commit after a fold costs what it touches.
//!
//! - [`CowVec`]: a vector in chunks of [`CHUNK`] elements.
//! - [`CowMap`]: an ordered map, a B+tree of `Arc`ed nodes.
//! - [`IdSet`]: an ordered set of node ids, inline while small.

use std::collections::HashMap;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

const CHUNK_BITS: usize = 7;
/// Elements per [`CowVec`] chunk.
pub const CHUNK: usize = 1 << CHUNK_BITS;
const CHUNK_MASK: usize = CHUNK - 1;

/// A vector stored as `Arc`ed chunks of [`CHUNK`] elements (the last one may
/// be shorter). A clone shares every chunk; a write copies the one chunk it
/// changes if that chunk is shared.
#[derive(Debug)]
pub struct CowVec<T> {
    chunks: Vec<Arc<Vec<T>>>,
    len: usize,
}

impl<T> Clone for CowVec<T> {
    fn clone(&self) -> Self {
        Self {
            chunks: self.chunks.clone(),
            len: self.len,
        }
    }
}

impl<T> Default for CowVec<T> {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            len: 0,
        }
    }
}

impl<T: Clone> CowVec<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> Option<&T> {
        self.chunks.get(i >> CHUNK_BITS)?.get(i & CHUNK_MASK)
    }

    /// The element at `i`, its chunk copied first if shared.
    #[inline]
    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        let chunk = self.chunks.get_mut(i >> CHUNK_BITS)?;
        if i & CHUNK_MASK >= chunk.len() {
            return None;
        }
        Arc::make_mut(chunk).get_mut(i & CHUNK_MASK)
    }

    /// Overwrite the element at `i`. Panics when `i` is out of bounds.
    #[inline]
    pub fn set(&mut self, i: usize, value: T) {
        let len = self.len;
        *self
            .get_mut(i)
            .unwrap_or_else(|| panic!("CowVec index {i} out of bounds (len {len})")) = value;
    }

    /// Replace the element at `i`, returning the old one. Panics when `i` is
    /// out of bounds.
    pub fn replace(&mut self, i: usize, value: T) -> T {
        let len = self.len;
        std::mem::replace(
            self.get_mut(i)
                .unwrap_or_else(|| panic!("CowVec index {i} out of bounds (len {len})")),
            value,
        )
    }

    pub fn push(&mut self, value: T) {
        match self.chunks.last_mut() {
            Some(last) if last.len() < CHUNK => Arc::make_mut(last).push(value),
            _ => {
                let mut chunk = Vec::with_capacity(CHUNK);
                chunk.push(value);
                self.chunks.push(Arc::new(chunk));
            }
        }
        self.len += 1;
    }

    /// Grow to `len` elements, filling with `fill`. Never shrinks.
    pub fn grow_to(&mut self, len: usize, fill: T) {
        while self.len < len {
            let room = match self.chunks.last() {
                Some(last) if last.len() < CHUNK => CHUNK - last.len(),
                _ => {
                    self.chunks.push(Arc::new(Vec::with_capacity(CHUNK)));
                    CHUNK
                }
            };
            let add = room.min(len - self.len);
            let last = self.chunks.last_mut().expect("a chunk was just ensured");
            let last = Arc::make_mut(last);
            last.resize(last.len() + add, fill.clone());
            self.len += add;
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.chunks.iter().flat_map(|c| c.iter())
    }

    pub fn from_vec(v: Vec<T>) -> Self {
        let len = v.len();
        if len <= CHUNK {
            return Self {
                chunks: if len == 0 {
                    Vec::new()
                } else {
                    vec![Arc::new(v)]
                },
                len,
            };
        }
        Self {
            chunks: v.chunks(CHUNK).map(|c| Arc::new(c.to_vec())).collect(),
            len,
        }
    }
}

impl<T: Clone> std::ops::Index<usize> for CowVec<T> {
    type Output = T;

    #[inline]
    fn index(&self, i: usize) -> &T {
        let len = self.len;
        self.get(i)
            .unwrap_or_else(|| panic!("CowVec index {i} out of bounds (len {len})"))
    }
}

impl<T: Clone> FromIterator<T> for CowVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut v = Self::new();
        for x in iter {
            v.push(x);
        }
        v
    }
}

impl<T: Clone + PartialEq> PartialEq for CowVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

/// Most entries a [`CowMap`] node holds before it splits.
pub const NODE_MAX: usize = 32;

/// Deepest a [`CowMap`] can grow: a level is added only when a full root
/// splits, and every split leaves at least `NODE_MAX / 2` entries a side, so
/// 2^32 keys fit in far fewer levels than this.
const MAX_DEPTH: usize = 32;

/// A [`CowMap`] node. Every node but an empty map's root is non-empty. Keys
/// sit apart from what they hold, so a search reads one compact array.
#[derive(Debug)]
enum Node<K, V> {
    /// `keys` ascending; `vals[i]` is the value of `keys[i]`.
    Leaf { keys: Vec<K>, vals: Vec<V> },
    /// Child `i` holds keys `>= bounds[i]` and `< bounds[i + 1]`. Child 0
    /// also takes every key below its bound, so that bound is never consulted.
    Internal {
        bounds: Vec<K>,
        kids: Vec<Arc<Node<K, V>>>,
    },
}

/// A copy has room for the insert that made it, so it does not reallocate.
fn copy_with_room<T: Clone>(items: &[T]) -> Vec<T> {
    let mut copy = Vec::with_capacity(NODE_MAX + 1);
    copy.extend_from_slice(items);
    copy
}

impl<K: Clone, V: Clone> Clone for Node<K, V> {
    fn clone(&self) -> Self {
        match self {
            Node::Leaf { keys, vals } => Node::Leaf {
                keys: copy_with_room(keys),
                vals: copy_with_room(vals),
            },
            Node::Internal { bounds, kids } => Node::Internal {
                bounds: copy_with_room(bounds),
                kids: copy_with_room(kids),
            },
        }
    }
}

impl<K, V> Node<K, V> {
    fn is_empty(&self) -> bool {
        match self {
            Node::Leaf { keys, .. } => keys.is_empty(),
            Node::Internal { kids, .. } => kids.is_empty(),
        }
    }

    fn first_key(&self) -> &K {
        match self {
            Node::Leaf { keys, .. } => &keys[0],
            Node::Internal { bounds, .. } => &bounds[0],
        }
    }
}

/// The child that `k` belongs in.
fn route<K: Ord>(bounds: &[K], k: &K) -> usize {
    bounds.partition_point(|bound| bound <= k).saturating_sub(1)
}

/// The child indices from the root down to the leaf holding `k`, and `k`'s
/// position in that leaf.
struct Path {
    kids: [u8; MAX_DEPTH],
    depth: usize,
    at: usize,
}

/// A persistent ordered map: a B+tree whose nodes (at most [`NODE_MAX`]
/// entries) sit behind `Arc`s. A clone shares the whole tree; a write copies
/// the nodes on its path that are shared — a few dozen entries — and leaves
/// every other node shared with the clone.
///
/// Nodes split when full and are dropped when empty; an underfull node is
/// not merged, which keeps a delete to its own path.
#[derive(Debug)]
pub struct CowMap<K, V> {
    root: Option<Arc<Node<K, V>>>,
    len: usize,
}

impl<K, V> Clone for CowMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            len: self.len,
        }
    }
}

impl<K, V> Default for CowMap<K, V> {
    fn default() -> Self {
        Self { root: None, len: 0 }
    }
}

/// A node that split off the right of a full one, with its lower bound.
type Split<K, V> = Option<(K, Arc<Node<K, V>>)>;

fn insert_into<K: Ord + Clone, V: Clone>(
    node: &mut Arc<Node<K, V>>,
    k: K,
    v: V,
) -> (Option<V>, Split<K, V>) {
    match Arc::make_mut(node) {
        Node::Leaf { keys, vals } => match keys.binary_search(&k) {
            Ok(i) => (Some(std::mem::replace(&mut vals[i], v)), None),
            Err(i) => {
                keys.insert(i, k);
                vals.insert(i, v);
                if keys.len() <= NODE_MAX {
                    return (None, None);
                }
                let half = keys.len() / 2;
                let (rk, rv) = (keys.split_off(half), vals.split_off(half));
                let bound = rk[0].clone();
                (
                    None,
                    Some((bound, Arc::new(Node::Leaf { keys: rk, vals: rv }))),
                )
            }
        },
        Node::Internal { bounds, kids } => {
            let i = route(bounds, &k);
            let (old, split) = insert_into(&mut kids[i], k, v);
            if let Some((bound, right)) = split {
                bounds.insert(i + 1, bound);
                kids.insert(i + 1, right);
            }
            if kids.len() <= NODE_MAX {
                return (old, None);
            }
            let half = kids.len() / 2;
            let (rb, rk) = (bounds.split_off(half), kids.split_off(half));
            let bound = rb[0].clone();
            (
                old,
                Some((
                    bound,
                    Arc::new(Node::Internal {
                        bounds: rb,
                        kids: rk,
                    }),
                )),
            )
        }
    }
}

/// Remove the entry `path` leads to, dropping any node the removal empties.
fn remove_at<K: Clone, V: Clone>(node: &mut Arc<Node<K, V>>, kids_path: &[u8], at: usize) -> V {
    match Arc::make_mut(node) {
        Node::Leaf { keys, vals } => {
            keys.remove(at);
            vals.remove(at)
        }
        Node::Internal { bounds, kids } => {
            let i = kids_path[0] as usize;
            let old = remove_at(&mut kids[i], &kids_path[1..], at);
            if kids[i].is_empty() {
                kids.remove(i);
                bounds.remove(i);
            }
            old
        }
    }
}

/// Group `level` into parents of about three quarters of [`NODE_MAX`].
fn build_parents<K: Clone, V>(level: Vec<(K, Arc<Node<K, V>>)>) -> Vec<(K, Arc<Node<K, V>>)> {
    let per = NODE_MAX * 3 / 4;
    let mut parents = Vec::with_capacity(level.len().div_ceil(per));
    let mut level = level.into_iter().peekable();
    while level.peek().is_some() {
        let (bounds, kids): (Vec<K>, Vec<_>) = level.by_ref().take(per).unzip();
        parents.push((bounds[0].clone(), Arc::new(Node::Internal { bounds, kids })));
    }
    parents
}

impl<K: Ord + Clone, V: Clone> CowMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Where `k` is, if it is present.
    fn find(&self, k: &K) -> Option<Path> {
        let mut path = Path {
            kids: [0; MAX_DEPTH],
            depth: 0,
            at: 0,
        };
        let mut node = self.root.as_deref()?;
        loop {
            match node {
                Node::Leaf { keys, .. } => {
                    path.at = keys.binary_search(k).ok()?;
                    return Some(path);
                }
                Node::Internal { bounds, kids } => {
                    let i = route(bounds, k);
                    path.kids[path.depth] = i as u8;
                    path.depth += 1;
                    node = &kids[i];
                }
            }
        }
    }

    pub fn get(&self, k: &K) -> Option<&V> {
        let mut node = self.root.as_deref()?;
        loop {
            match node {
                Node::Leaf { keys, vals } => return Some(&vals[keys.binary_search(k).ok()?]),
                Node::Internal { bounds, kids } => node = &kids[route(bounds, k)],
            }
        }
    }

    pub fn contains_key(&self, k: &K) -> bool {
        self.get(k).is_some()
    }

    /// The value at `k`, its path copied first where shared. A missing key
    /// copies nothing.
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        let path = self.find(k)?;
        let mut node = Arc::make_mut(self.root.as_mut()?);
        for &i in &path.kids[..path.depth] {
            let Node::Internal { kids, .. } = node else {
                unreachable!("a path descends through internal nodes");
            };
            node = Arc::make_mut(&mut kids[i as usize]);
        }
        let Node::Leaf { vals, .. } = node else {
            unreachable!("a path ends at a leaf");
        };
        Some(&mut vals[path.at])
    }

    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let Some(root) = self.root.as_mut() else {
            self.root = Some(Arc::new(Node::Leaf {
                keys: vec![k],
                vals: vec![v],
            }));
            self.len = 1;
            return None;
        };
        let (old, split) = insert_into(root, k, v);
        if let Some((bound, right)) = split {
            let left = self.root.take().expect("the root just split");
            let first = left.first_key().clone();
            self.root = Some(Arc::new(Node::Internal {
                bounds: vec![first, bound],
                kids: vec![left, right],
            }));
        }
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    /// The value at `k`, inserted from `default` first when missing.
    pub fn get_or_insert_with(&mut self, k: K, default: impl FnOnce() -> V) -> &mut V {
        if !self.contains_key(&k) {
            self.insert(k.clone(), default());
        }
        self.get_mut(&k).expect("just inserted")
    }

    /// Remove `k`. A missing key copies nothing.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let path = self.find(k)?;
        let old = remove_at(self.root.as_mut()?, &path.kids[..path.depth], path.at);
        self.len -= 1;
        // An emptied root goes, and a root with one child hands over to it.
        loop {
            match self.root.as_deref() {
                Some(root) if root.is_empty() => self.root = None,
                Some(Node::Internal { kids, .. }) if kids.len() == 1 => {
                    self.root = Some(Arc::clone(&kids[0]));
                }
                _ => break,
            }
        }
        Some(old)
    }

    pub fn iter(&self) -> Range<'_, K, V> {
        self.range::<std::ops::RangeFull>(..)
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> + '_ {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> + '_ {
        self.iter().map(|(_, v)| v)
    }

    /// The entries within `range`, in key order. Panics on an inverted range,
    /// as `BTreeMap::range` does.
    pub fn range<R: RangeBounds<K>>(&self, range: R) -> Range<'_, K, V> {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();
        if let (Bound::Included(s) | Bound::Excluded(s), Bound::Included(e) | Bound::Excluded(e)) =
            (&start, &end)
        {
            let both_excluded = matches!((&start, &end), (Bound::Excluded(_), Bound::Excluded(_)));
            assert!(
                s < e || (s == e && !both_excluded),
                "range start is greater than range end in CowMap"
            );
        }
        let mut it = Range {
            stack: Vec::new(),
            keys: &[],
            vals: &[],
            end,
        };
        let Some(mut node) = self.root.as_deref() else {
            return it;
        };
        loop {
            match node {
                Node::Internal { bounds, kids } => {
                    let i = match &start {
                        Bound::Included(k) | Bound::Excluded(k) => route(bounds, k),
                        Bound::Unbounded => 0,
                    };
                    it.stack.push((kids, i + 1));
                    node = &kids[i];
                }
                Node::Leaf { keys, vals } => {
                    let from = match &start {
                        Bound::Included(k) => keys.partition_point(|x| x < k),
                        Bound::Excluded(k) => keys.partition_point(|x| x <= k),
                        Bound::Unbounded => 0,
                    };
                    it.keys = &keys[from..];
                    it.vals = &vals[from..];
                    return it;
                }
            }
        }
    }

    /// Build from entries already in strictly ascending key order.
    pub fn from_sorted(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let per = NODE_MAX * 3 / 4;
        let mut len = 0;
        let mut level: Vec<(K, Arc<Node<K, V>>)> = Vec::new();
        let (mut keys, mut vals): (Vec<K>, Vec<V>) = (Vec::new(), Vec::new());
        let flush =
            |keys: &mut Vec<K>, vals: &mut Vec<V>, level: &mut Vec<(K, Arc<Node<K, V>>)>| {
                let keys = std::mem::take(keys);
                let vals = std::mem::take(vals);
                level.push((keys[0].clone(), Arc::new(Node::Leaf { keys, vals })));
            };
        for (k, v) in entries {
            debug_assert!(
                keys.last()
                    .or_else(|| level.last().map(|(_, n)| last_key(n)))
                    .is_none_or(|last| *last < k),
                "CowMap::from_sorted needs strictly ascending keys"
            );
            keys.push(k);
            vals.push(v);
            len += 1;
            if keys.len() == per {
                flush(&mut keys, &mut vals, &mut level);
            }
        }
        if !keys.is_empty() {
            flush(&mut keys, &mut vals, &mut level);
        }
        while level.len() > 1 {
            level = build_parents(level);
        }
        Self {
            root: level.pop().map(|(_, node)| node),
            len,
        }
    }

    /// How many of this map's nodes `other` does not share, and how many it
    /// has in all: what a write since a clone copied.
    #[cfg(test)]
    fn unshared_nodes(&self, other: &Self) -> (usize, usize) {
        fn walk<K, V>(node: &Arc<Node<K, V>>, out: &mut Vec<*const Node<K, V>>) {
            out.push(Arc::as_ptr(node));
            if let Node::Internal { kids, .. } = &**node {
                for kid in kids {
                    walk(kid, out);
                }
            }
        }
        let (mut mine, mut theirs) = (Vec::new(), Vec::new());
        self.root.iter().for_each(|r| walk(r, &mut mine));
        other.root.iter().for_each(|r| walk(r, &mut theirs));
        let theirs: std::collections::HashSet<_> = theirs.into_iter().collect();
        let unshared = mine.iter().filter(|p| !theirs.contains(p)).count();
        (unshared, mine.len())
    }

    /// Levels from the root to a leaf.
    #[cfg(test)]
    fn depth(&self) -> usize {
        let mut depth = 0;
        let mut node = self.root.as_deref();
        while let Some(n) = node {
            depth += 1;
            node = match n {
                Node::Leaf { .. } => None,
                Node::Internal { kids, .. } => Some(&kids[0]),
            };
        }
        depth
    }

    /// Panics unless every node is non-empty and at most [`NODE_MAX`] wide,
    /// and every key lies within its child's bounds.
    #[cfg(test)]
    fn check(&self) {
        fn walk<K: Ord, V>(node: &Node<K, V>, lo: Option<&K>, hi: Option<&K>) -> usize {
            assert!(!node.is_empty(), "an empty node");
            let within = |k: &K| lo.is_none_or(|l| l <= k) && hi.is_none_or(|h| k < h);
            match node {
                Node::Leaf { keys, vals } => {
                    assert!(keys.len() <= NODE_MAX && keys.len() == vals.len());
                    assert!(keys.windows(2).all(|w| w[0] < w[1]));
                    assert!(keys.iter().all(within));
                    keys.len()
                }
                Node::Internal { bounds, kids } => {
                    assert!(kids.len() <= NODE_MAX && kids.len() == bounds.len());
                    (0..kids.len())
                        .map(|i| {
                            let lo = if i == 0 { lo } else { Some(&bounds[i]) };
                            let hi = bounds.get(i + 1).or(hi);
                            walk(&kids[i], lo, hi)
                        })
                        .sum()
                }
            }
        }
        let n = self.root.as_deref().map_or(0, |r| walk(r, None, None));
        assert_eq!(n, self.len);
    }
}

fn last_key<K, V>(node: &Node<K, V>) -> &K {
    match node {
        Node::Leaf { keys, .. } => keys.last().expect("a non-empty leaf"),
        Node::Internal { kids, .. } => last_key(kids.last().expect("a non-empty node")),
    }
}

/// Children of an internal node a [`Range`] is under, and the next to visit.
type Frame<'a, K, V> = (&'a [Arc<Node<K, V>>], usize);

/// The entries of a [`CowMap`] within a range, in key order.
pub struct Range<'a, K, V> {
    /// The internal nodes above the current leaf, each with the index of the
    /// next child to visit.
    stack: Vec<Frame<'a, K, V>>,
    /// What is left of the current leaf.
    keys: &'a [K],
    vals: &'a [V],
    end: Bound<K>,
}

impl<'a, K: Ord, V> Iterator for Range<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let (Some((k, keys)), Some((v, vals))) =
                (self.keys.split_first(), self.vals.split_first())
            {
                let past = match &self.end {
                    Bound::Included(e) => k > e,
                    Bound::Excluded(e) => k >= e,
                    Bound::Unbounded => false,
                };
                if past {
                    self.stack.clear();
                    self.keys = &[];
                    self.vals = &[];
                    return None;
                }
                self.keys = keys;
                self.vals = vals;
                return Some((k, v));
            }
            // The leaf is spent: the next one is the leftmost leaf under the
            // nearest ancestor's next child.
            let (kids, next) = self.stack.last_mut()?;
            if *next >= kids.len() {
                self.stack.pop();
                continue;
            }
            let mut node: &'a Node<K, V> = &kids[*next];
            *next += 1;
            loop {
                match node {
                    Node::Leaf { keys, vals } => {
                        self.keys = keys;
                        self.vals = vals;
                        break;
                    }
                    Node::Internal { kids, .. } => {
                        self.stack.push((kids, 1));
                        node = &kids[0];
                    }
                }
            }
        }
    }
}

impl<K: Ord + Clone, V: Clone> FromIterator<(K, V)> for CowMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (k, v) in iter {
            map.insert(k, v);
        }
        map
    }
}

impl<K: Ord + Clone, V: Clone + PartialEq> PartialEq for CowMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

/// Most ids an [`IdSet`] holds in one sorted vector before it moves into a
/// [`CowMap`].
const SMALL_MAX: usize = 64;

/// An ordered set of node ids. One id — the usual posting of a field whose
/// values are distinct — is held inline and clones for free; up to 64 sit in
/// one sorted vector; beyond that a [`CowMap`], so a write to a large shared
/// posting copies its path, not the set.
#[derive(Debug, Clone)]
pub enum IdSet {
    One(u32),
    /// Sorted, and never exactly one id.
    Small(Vec<u32>),
    Large(CowMap<u32, ()>),
}

impl Default for IdSet {
    fn default() -> Self {
        IdSet::Small(Vec::new())
    }
}

impl IdSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        match self {
            IdSet::One(_) => 1,
            IdSet::Small(v) => v.len(),
            IdSet::Large(m) => m.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, id: u32) -> bool {
        match self {
            IdSet::One(x) => *x == id,
            IdSet::Small(v) => v.binary_search(&id).is_ok(),
            IdSet::Large(m) => m.contains_key(&id),
        }
    }

    /// Insert `id`; `true` when it was not there.
    pub fn insert(&mut self, id: u32) -> bool {
        match self {
            IdSet::One(x) if *x == id => false,
            IdSet::One(x) => {
                let x = *x;
                *self = IdSet::Small(if x < id { vec![x, id] } else { vec![id, x] });
                true
            }
            IdSet::Small(v) if v.is_empty() => {
                *self = IdSet::One(id);
                true
            }
            IdSet::Small(v) => match v.binary_search(&id) {
                Ok(_) => false,
                Err(i) if v.len() < SMALL_MAX => {
                    v.insert(i, id);
                    true
                }
                Err(_) => {
                    let mut m: CowMap<u32, ()> = CowMap::from_sorted(v.iter().map(|&i| (i, ())));
                    m.insert(id, ());
                    *self = IdSet::Large(m);
                    true
                }
            },
            IdSet::Large(m) => m.insert(id, ()).is_none(),
        }
    }

    /// Remove `id`; `true` when it was there.
    pub fn remove(&mut self, id: u32) -> bool {
        match self {
            IdSet::One(x) if *x == id => {
                *self = IdSet::Small(Vec::new());
                true
            }
            IdSet::One(_) => false,
            IdSet::Small(v) => match v.binary_search(&id) {
                Ok(i) => {
                    v.remove(i);
                    if let [only] = v[..] {
                        *self = IdSet::One(only);
                    }
                    true
                }
                Err(_) => false,
            },
            IdSet::Large(m) => m.remove(&id).is_some(),
        }
    }

    /// The ids, ascending.
    pub fn iter(&self) -> IdSetIter<'_> {
        match self {
            IdSet::One(x) => IdSetIter::Slice(std::slice::from_ref(x).iter()),
            IdSet::Small(v) => IdSetIter::Slice(v.iter()),
            IdSet::Large(m) => IdSetIter::Large(m.iter()),
        }
    }
}

impl PartialEq for IdSet {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl FromIterator<u32> for IdSet {
    fn from_iter<I: IntoIterator<Item = u32>>(iter: I) -> Self {
        let mut set = IdSet::new();
        for id in iter {
            set.insert(id);
        }
        set
    }
}

/// The ids of an [`IdSet`], ascending.
pub enum IdSetIter<'a> {
    Slice(std::slice::Iter<'a, u32>),
    Large(Range<'a, u32, ()>),
}

impl Iterator for IdSetIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        match self {
            IdSetIter::Slice(it) => it.next().copied(),
            IdSetIter::Large(it) => it.next().map(|(id, ())| *id),
        }
    }
}

/// Hashes a `u64` key that is already a hash, as itself.
#[derive(Default)]
struct PassThrough(u64);

impl std::hash::Hasher for PassThrough {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(b);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

type ByHash = HashMap<u64, u32, std::hash::BuildHasherDefault<PassThrough>>;

/// Shards of a [`HashIndex`], chosen by a hash's top bits.
const HASH_SHARDS: usize = 1024;

#[cfg(test)]
thread_local! {
    /// When set, every string hashes to this: the tests' way to make every
    /// key collide.
    pub(crate) static FORCED_HASH: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    #[cfg(test)]
    if let Some(h) = FORCED_HASH.with(|f| f.get()) {
        return h;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Strings to dense ids, for a caller that keeps the strings itself (a
/// [`CowVec`] indexed by id): `resolve(id)` gives the string `id` stands for.
///
/// A string is found by its hash, in one of 1024 shared shards, and confirmed
/// through `resolve`; the rare string whose hash a different string already
/// holds is kept by value in `collided`. So a clone copies no string, and a
/// write copies one shard of about `len / 1024` hashes.
#[derive(Debug, Clone, Default)]
pub struct HashIndex {
    shards: Vec<Arc<ByHash>>,
    collided: Arc<HashMap<String, u32>>,
    len: usize,
}

impl HashIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many strings are indexed.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn shard_of(h: u64) -> usize {
        (h >> 54) as usize
    }

    /// The hash [`Self::get_hashed`] and [`Self::insert_hashed`] take, so a
    /// lookup that misses and then inserts hashes the string once.
    pub fn hash(key: &str) -> u64 {
        hash_str(key)
    }

    pub fn get<'a>(&self, key: &str, resolve: impl Fn(u32) -> Option<&'a str>) -> Option<u32> {
        self.get_hashed(key, hash_str(key), resolve)
    }

    /// [`Self::get`], given `key`'s [`Self::hash`].
    pub fn get_hashed<'a>(
        &self,
        key: &str,
        h: u64,
        resolve: impl Fn(u32) -> Option<&'a str>,
    ) -> Option<u32> {
        if let Some(&id) = self.shards.get(Self::shard_of(h)).and_then(|s| s.get(&h)) {
            if resolve(id) == Some(key) {
                return Some(id);
            }
        }
        if self.collided.is_empty() {
            return None;
        }
        self.collided.get(key).copied()
    }

    /// Index `key` as `id`. The caller has checked `key` is not indexed, and
    /// `resolve` must already answer every other indexed id.
    pub fn insert(&mut self, key: &str, id: u32) {
        self.insert_hashed(key, hash_str(key), id);
    }

    /// [`Self::insert`], given `key`'s [`Self::hash`].
    pub fn insert_hashed(&mut self, key: &str, h: u64, id: u32) {
        if self.shards.is_empty() {
            self.shards = (0..HASH_SHARDS).map(|_| Arc::default()).collect();
        }
        let shard = &mut self.shards[Self::shard_of(h)];
        if shard.contains_key(&h) {
            Arc::make_mut(&mut self.collided).insert(key.to_string(), id);
        } else {
            Arc::make_mut(shard).insert(h, id);
        }
        self.len += 1;
    }

    /// Stop indexing `key`, returning its id. `resolve` must still answer
    /// `key`'s id with `key`.
    pub fn remove<'a>(
        &mut self,
        key: &str,
        resolve: impl Fn(u32) -> Option<&'a str>,
    ) -> Option<u32> {
        let h = hash_str(key);
        let shard_of = Self::shard_of(h);
        if let Some(&id) = self.shards.get(shard_of).and_then(|s| s.get(&h)) {
            if resolve(id) == Some(key) {
                Arc::make_mut(&mut self.shards[shard_of]).remove(&h);
                self.len -= 1;
                return Some(id);
            }
        }
        if !self.collided.contains_key(key) {
            return None;
        }
        self.len -= 1;
        Arc::make_mut(&mut self.collided).remove(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A tiny deterministic generator.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn a_cow_vec_reads_and_writes_as_a_vec_across_chunks() {
        let mut v: CowVec<u64> = CowVec::new();
        let mut plain: Vec<u64> = Vec::new();
        let mut r = Rng(0x1234_5678_9ABC_DEF1);
        for step in 0..20_000 {
            match r.next() % 4 {
                0 => {
                    let x = r.next();
                    v.push(x);
                    plain.push(x);
                }
                1 => {
                    let to = plain.len() + (r.next() % 3000) as usize;
                    v.grow_to(to, step);
                    plain.resize(to, step);
                }
                _ if !plain.is_empty() => {
                    let i = (r.next() % plain.len() as u64) as usize;
                    v.set(i, step);
                    plain[i] = step;
                }
                _ => {}
            }
        }
        assert_eq!(v.len(), plain.len());
        assert!(v.iter().eq(plain.iter()));
        assert_eq!(CowVec::from_vec(plain.clone()), v);
        assert_eq!(v.get(plain.len()), None);
        for (i, x) in plain.iter().enumerate().step_by(97) {
            assert_eq!(v[i], *x);
        }
    }

    #[test]
    fn a_write_to_a_cloned_vec_copies_one_chunk_and_leaves_the_clone_alone() {
        let v: CowVec<u32> = (0..(10 * CHUNK as u32)).collect();
        let mut w = v.clone();
        w.set(3 * CHUNK + 5, 999);
        let shared = v
            .chunks
            .iter()
            .zip(&w.chunks)
            .filter(|(a, b)| Arc::ptr_eq(a, b))
            .count();
        assert_eq!(shared, 9, "only the written chunk is copied");
        assert_eq!(v[3 * CHUNK + 5], (3 * CHUNK + 5) as u32);
        assert_eq!(w[3 * CHUNK + 5], 999);
    }

    #[test]
    fn a_cow_map_answers_as_a_btree_map_over_random_writes_and_ranges() {
        let mut m: CowMap<u32, u64> = CowMap::new();
        let mut plain: BTreeMap<u32, u64> = BTreeMap::new();
        let mut r = Rng(0x0BAD_5EED_F00D_CAFE);
        let mut snapshots = Vec::new();
        for step in 0..30_000u64 {
            let k = (r.next() % 5_000) as u32;
            if r.next().is_multiple_of(3) {
                assert_eq!(m.remove(&k), plain.remove(&k));
            } else {
                assert_eq!(m.insert(k, step), plain.insert(k, step));
            }
            if step % 7_000 == 0 {
                snapshots.push((m.clone(), plain.clone()));
            }
        }
        assert_eq!(m.len(), plain.len());
        assert!(m.iter().eq(plain.iter()));
        for _ in 0..500 {
            let a = (r.next() % 5_200) as u32;
            let b = a + (r.next() % 900) as u32;
            assert!(m.range(a..b).eq(plain.range(a..b)), "{a}..{b}");
            assert!(m.range(a..=b).eq(plain.range(a..=b)), "{a}..={b}");
            assert!(m.range(a..).eq(plain.range(a..)), "{a}..");
            assert!(m.range(..b).eq(plain.range(..b)), "..{b}");
            assert_eq!(m.get(&a), plain.get(&a));
        }
        // Every clone kept its own contents while the original moved on.
        for (snap, at) in snapshots {
            assert!(snap.iter().eq(at.iter()));
        }
        m.check();
        let rebuilt: CowMap<u32, u64> = CowMap::from_sorted(plain.clone());
        assert_eq!(rebuilt, m);
    }

    #[test]
    fn a_write_to_a_cloned_map_copies_only_its_path() {
        let m: CowMap<u32, ()> = CowMap::from_sorted((0..1_000_000).map(|i| (i * 2, ())));
        m.check();
        // One write and its path: a leaf and its ancestors, four levels here.
        let mut w = m.clone();
        w.insert(500_001, ());
        let (copied, total) = w.unshared_nodes(&m);
        let depth = m.depth();
        assert!(depth <= 7, "a million keys in {depth} levels");
        // Its path, plus the node a split adds.
        assert!(
            copied <= depth + 1,
            "{copied} of {total} nodes copied by one insert"
        );
        assert!(total > 40_000);
        // A missing key copies nothing.
        let mut x = m.clone();
        assert_eq!(x.remove(&7), None);
        assert!(x.get_mut(&9).is_none());
        assert_eq!(x.unshared_nodes(&m).0, 0);
        // A hundred writes copy at most their hundred paths.
        for i in 0..100u32 {
            w.remove(&(i * 9_998));
            w.insert(i * 10_001 + 1, ());
        }
        let (copied, _) = w.unshared_nodes(&m);
        assert!(
            copied <= (depth + 1) * 201,
            "{copied} nodes copied by 201 writes"
        );
        w.check();
        assert!(m.contains_key(&9_998) && !w.contains_key(&9_998));
        assert!(m.iter().eq((0..1_000_000)
            .map(|i| i * 2)
            .collect::<Vec<u32>>()
            .iter()
            .map(|k| (k, &()))));
    }

    #[test]
    fn a_map_emptied_and_refilled_stays_well_formed() {
        let mut m: CowMap<u32, u32> = CowMap::from_sorted((0..5_000).map(|i| (i, i)));
        let keep = m.clone();
        for i in (0..5_000).rev() {
            assert_eq!(m.remove(&i), Some(i));
            if i % 500 == 0 {
                m.check();
            }
        }
        assert!(m.is_empty() && m.iter().next().is_none());
        for i in 0..100 {
            m.insert(i * 3, i);
        }
        m.check();
        assert_eq!(
            m.range(10..20).map(|(k, _)| *k).collect::<Vec<_>>(),
            [12, 15, 18]
        );
        assert_eq!(keep.len(), 5_000);
        keep.check();
    }

    #[test]
    fn an_id_set_moves_inline_to_shared_and_answers_alike() {
        let mut set = IdSet::new();
        let mut plain = std::collections::BTreeSet::new();
        let mut r = Rng(42);
        for _ in 0..5_000 {
            let id = (r.next() % 400) as u32;
            if r.next().is_multiple_of(4) {
                assert_eq!(set.remove(id), plain.remove(&id));
            } else {
                assert_eq!(set.insert(id), plain.insert(id));
            }
            assert_eq!(set.len(), plain.len());
        }
        assert!(matches!(set, IdSet::Large(_)));
        let mut one = IdSet::new();
        assert!(one.insert(7) && matches!(one, IdSet::One(7)) && !one.insert(7));
        assert!(one.insert(3) && one.iter().eq([3, 7]));
        assert!(one.remove(3) && matches!(one, IdSet::One(7)));
        assert!(one.remove(7) && one.is_empty() && !one.contains(7));
        assert!(set.iter().eq(plain.iter().copied()));
        assert!(plain.iter().all(|&id| set.contains(id)));
    }
}
