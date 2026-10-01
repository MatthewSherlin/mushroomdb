//! Identity: the alias set `SAME_AS` compares.
//!
//! `SAME_AS` is a rule, and a rule can only compare what a node holds. The
//! engine's `Overlap` predicate is an exact Jaccard over list tokens — no case
//! folding, no tokenising — so `"Matt"` and `"matt"` never meet unless both
//! sides were written the same way. This module is the one place that writes
//! them the same way: every entity `remember` or `upsert_entity` touches gets an
//! `aliases` list built here, from its key and its current `name` and nothing
//! else, normalised identically on every node.
//!
//! The list is recomputed on each describing write, not accumulated: it is a
//! function of the key and the name as they stand, sorted and deduplicated, so
//! a node rewritten with the same inputs is not rewritten at all, and a
//! renamed node stops matching its old name.
//!
//! A second list, `alias_keys`, holds what a caller declared, as declared, and
//! only there. Declared aliases are kept out of `aliases` for two reasons.
//! Jaccard punishes unequal sets: every full-name link sits exactly on the
//! floor, 3/5, and one alias declared on one side would make it 3/6 and
//! retract it. And `aliases` holds derived name words, which must not act as
//! claims: an entity merely *named* "Alex" would otherwise claim every stub
//! keyed `alex`. The declared list is what a `KeyMatch` rule reads instead —
//! an entity that names a stub's key links it exactly. So two entities that
//! declare the same alias gain no overlap from it; a declared alias links
//! only through a claim on a stub's key.

use crate::GraphDb;
use core_storage::fs::Fs;
use core_storage::{GraphError, Result, Value};
use std::collections::BTreeSet;
use unicode_normalization::UnicodeNormalization;

/// The property every entity's alias list lives in.
pub const ALIASES_FIELD: &str = "aliases";

/// The property an entity's declared aliases live in, as the caller wrote
/// them: the list the identity preset's `KeyMatch` rules read.
pub const ALIAS_KEYS_FIELD: &str = "alias_keys";

/// The edge type the identity preset derives.
pub const SAME_AS_EDGE: &str = "SAME_AS";

/// The edge property a `SAME_AS` edge carries its Jaccard score in.
pub const SAME_AS_WEIGHT: &str = "weight";

/// The lowest alias-set Jaccard the identity preset links at.
///
/// Under [`derive_aliases`], two nodes that share a complete two-word name and
/// differ only in key score exactly 3/5 = 0.6; one shared word of such a name
/// scores at most 1/7. So 0.6 is the floor at which a link means "the same
/// full name", and below which it would start meaning "the same first name" —
/// the conservative setting the spec's O-3 settled on.
pub const SAME_AS_FLOOR: f64 = 0.6;

/// One `SAME_AS` claim: two keys, in byte order, and the score that links them.
///
/// A same-label rule derives both directions; this is the one claim they make.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SameAsPair {
    pub a: String,
    pub b: String,
    pub score: f64,
}

/// Every `SAME_AS` claim touching any of `keys`, as unordered pairs, scored.
///
/// Read through [`GraphDb::node_edges`], so a key that does not exist is
/// skipped rather than an error. An edge with no stored score — a caller's own
/// `SAME_AS` written through `query` — counts as 1.0. Of a pair's two
/// directions, the higher score is kept. Sorted by `(a, b)`.
pub fn same_as_pairs<F: Fs>(db: &GraphDb<F>, keys: &[String]) -> Vec<SameAsPair> {
    let mut best: std::collections::BTreeMap<(String, String), f64> =
        std::collections::BTreeMap::new();
    for key in keys {
        let Ok(edges) = db.node_edges(key) else {
            continue;
        };
        for e in edges.into_iter().filter(|e| e.edge_type == SAME_AS_EDGE) {
            let score = match db.get_edge_prop(SAME_AS_EDGE, &e.src_key, &e.dst_key, SAME_AS_WEIGHT)
            {
                Some(Value::Float(f)) => f,
                Some(Value::Int(i)) => i as f64,
                _ => 1.0,
            };
            let pair = if e.src_key <= e.dst_key {
                (e.src_key, e.dst_key)
            } else {
                (e.dst_key, e.src_key)
            };
            let slot = best.entry(pair).or_insert(score);
            if score > *slot {
                *slot = score;
            }
        }
    }
    best.into_iter()
        .map(|((a, b), score)| SameAsPair { a, b, score })
        .collect()
}

/// The claims in `before` that `after` no longer holds, each with the score it
/// had: the identity links a write retracted.
///
/// Both lists are [`same_as_pairs`] answers for the same keys, either side of
/// one write. A pair whose score only changed is still held, and is not here.
#[must_use]
pub fn same_as_lost(before: &[SameAsPair], after: &[SameAsPair]) -> Vec<SameAsPair> {
    before
        .iter()
        .filter(|b| !after.iter().any(|p| p.a == b.a && p.b == b.b))
        .cloned()
        .collect()
}

/// Most aliases one node may carry.
///
/// A real entity carries its key, its full name and that name's two to four
/// words — about six. Thirty-two is five times that, and it keeps one node
/// from becoming a bag of tokens that is a rule candidate for half the store
/// on every write. Only a name of more than thirty words can reach it.
pub const MAX_ALIASES: usize = 32;

/// `s` in Unicode NFC, lowercased, with every run of characters that are not
/// letters or digits collapsed to one space and the ends trimmed.
///
/// `"Matthew  Sherlin"`, `"matthew_sherlin"` and `"MATTHEW-SHERLIN"` all
/// become `"matthew sherlin"`. Letters are Unicode letters: `"José Ñúñez"`
/// becomes `"josé ñúñez"`.
///
/// NFC comes first, so a name typed in decomposed form — `e` followed by
/// U+0301, a combining accent — becomes the same `é` as its precomposed
/// spelling instead of splitting at the mark. A combining mark with no
/// precomposed form is still not a letter, and still separates words.
#[must_use]
pub fn canonical(s: &str) -> String {
    tokens(s).join(" ")
}

/// The words of `s`: maximal runs of letters and digits, in NFC, lowercased.
///
/// NFC is applied on both sides of the lowercasing. Before, so a decomposed
/// letter is one letter when it is lowercased and split. After, because
/// lowercasing can leave a sequence that composes further — `ᾼ` with an acute
/// lowercases to `ᾳ` beside the accent, which is `ᾴ` — and without the second
/// pass [`canonical`] would not be idempotent on it.
fn tokens(s: &str) -> Vec<String> {
    s.nfc()
        .collect::<String>()
        .to_lowercase()
        .nfc()
        .collect::<String>()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// The aliases a node's own fields imply — the whole of its `aliases` list.
///
/// - the key, lowercased — keys are identifiers, so they are not tokenised;
/// - the name in [`canonical`] form, and each of its words when it has more
///   than one.
///
/// Sorted and deduplicated. Empty strings never appear. An alias a caller
/// declares is not here: it goes to `alias_keys` ([`declared_alias_keys`]).
#[must_use]
pub fn derive_aliases(key: &str, name: Option<&str>) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let key = key.to_lowercase();
    if !key.trim().is_empty() {
        out.insert(key);
    }
    if let Some(name) = name {
        let words = tokens(name);
        if !words.is_empty() {
            out.insert(words.join(" "));
        }
        if words.len() > 1 {
            out.extend(words);
        }
    }
    out.into_iter().collect()
}

/// The list as a stored value.
#[must_use]
pub fn aliases_value(aliases: &[String]) -> Value {
    Value::List(aliases.iter().cloned().map(Value::Str).collect())
}

/// Most declared aliases one node may carry in `alias_keys`.
///
/// The same bound as [`MAX_ALIASES`]: a real entity has a few nicknames, and
/// thirty-two keeps one node from claiming half the store's stubs. It is far
/// below the engine's `MAX_KEYMATCH_LIST`, so a `KeyMatch` rule always reads
/// the whole list.
pub const MAX_ALIAS_KEYS: usize = MAX_ALIASES;

/// The aliases a caller declared, as keys a `KeyMatch` rule can compare:
/// trimmed and otherwise verbatim, blanks dropped, sorted and deduplicated.
///
/// Not lowercased and not tokenised. A key is an identifier and is matched
/// byte for byte, so `Matt` does not name a node keyed `matt`.
#[must_use]
pub fn declared_alias_keys(caller: &[String]) -> Vec<String> {
    let out: BTreeSet<String> = caller
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect();
    out.into_iter().collect()
}

/// The strings a stored `aliases` or `alias_keys` value holds: a string is
/// one, a list of strings is each. `None` for anything else — a number, a
/// map, a list holding one — which the caller refuses rather than replaces.
fn stored_strings(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        None => Some(Vec::new()),
        Some(Value::Str(s)) => Some(vec![s.clone()]),
        Some(Value::List(items)) => items
            .iter()
            .map(|item| match item {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        Some(_) => None,
    }
}

/// The refusal for a stored identity list that is not a string or a list of
/// strings: naming the node, the property, and the way out.
fn foreign_value(key: &str, field: &str) -> GraphError {
    GraphError::IngestError {
        detail: format!(
            "'{key}' already carries an '{field}' property that is not a string or \
             a list of strings; clear it with forget {{key: \"{key}\", prop: \
             \"{field}\"}} first"
        ),
    }
}

/// The two identity lists a node should carry, each `None` when the stored
/// value is already exactly that.
struct IdentityLists {
    aliases: Option<Vec<String>>,
    alias_keys: Option<Vec<String>>,
}

/// What `key`'s identity lists become when its name goes from `old_name` to
/// `new_name` and the caller declares `caller`.
///
/// `aliases` is recomputed, not accumulated: it is exactly
/// [`derive_aliases`] of the key and `new_name`. A changed or removed name
/// leaves nothing behind.
///
/// `alias_keys` accumulates: what it holds, what `caller` declares, and
/// whatever the stored `aliases` holds that the store did not derive. That
/// last part is how a user's own data survives. A node written before 0.7 may
/// carry an `aliases` property of its owner's, and a list this store wrote
/// before declared aliases left `aliases` holds them too. An item is the
/// store's when the key with `old_name`, or with `new_name`, implies it byte
/// for byte; every other item is moved to `alias_keys` as it stands — trimmed,
/// never canonicalised — on the first describing write, after which `aliases`
/// holds nothing to move. Only a blank item carries nothing and is let go.
///
/// The store can only compare against the name it finds. A name changed
/// underneath it by a raw write leaves its old words in `aliases`, and they
/// are then kept as declared; so is a former key `rename_node` left behind.
///
/// Refused, naming the node, when either stored value is not a string or a
/// list of strings, or when either list would exceed its cap.
fn identity_lists<F: Fs>(
    db: &GraphDb<F>,
    key: &str,
    old_name: Option<&str>,
    new_name: Option<&str>,
    caller: &[String],
) -> Result<IdentityLists> {
    let stored_aliases = db.get_prop(key, ALIASES_FIELD);
    let stored_keys = db.get_prop(key, ALIAS_KEYS_FIELD);
    let held_aliases =
        stored_strings(stored_aliases.as_ref()).ok_or_else(|| foreign_value(key, ALIASES_FIELD))?;
    let mut declared =
        stored_strings(stored_keys.as_ref()).ok_or_else(|| foreign_value(key, ALIAS_KEYS_FIELD))?;

    let derived = derive_aliases(key, new_name);
    if derived.len() > MAX_ALIASES {
        return Err(GraphError::IngestError {
            detail: format!(
                "'{key}' would carry {} aliases, more than {MAX_ALIASES}: its key, its name \
                 and each word of that name; give it a shorter name",
                derived.len()
            ),
        });
    }

    let was_derived = derive_aliases(key, old_name);
    let moved: Vec<String> = held_aliases
        .into_iter()
        .filter(|a| !derived.contains(a) && !was_derived.contains(a))
        .collect();
    let moved_count = declared_alias_keys(&moved).len();
    declared.extend(moved);
    declared.extend(caller.iter().cloned());
    let declared = declared_alias_keys(&declared);
    if declared.len() > MAX_ALIAS_KEYS {
        let carried = if moved_count > 0 {
            format!(
                " ({moved_count} of them carried over from its '{ALIASES_FIELD}' property; \
                 forget that to drop them)"
            )
        } else {
            String::new()
        };
        return Err(GraphError::IngestError {
            detail: format!(
                "'{key}' would carry {} declared aliases, more than {MAX_ALIAS_KEYS}{carried}; \
                 forget its '{ALIAS_KEYS_FIELD}' property first, or pass fewer",
                declared.len()
            ),
        });
    }

    let aliases = (stored_aliases.as_ref() != Some(&aliases_value(&derived))).then_some(derived);
    // Never written empty: a node that holds none and declares none carries
    // no `alias_keys` property at all.
    let alias_keys = if declared.is_empty() && stored_keys.is_none() {
        None
    } else {
        (stored_keys.as_ref() != Some(&aliases_value(&declared))).then_some(declared)
    };
    Ok(IdentityLists {
        aliases,
        alias_keys,
    })
}

/// [`identity_lists`] for a write that sets `props` on `key`: the name is the
/// one `props` sets, else the one the node already has. A `props` entry named
/// `aliases` or `alias_keys` is refused first — both lists are the store's.
fn identity_lists_after_write<F: Fs>(
    db: &GraphDb<F>,
    key: &str,
    props: &[(String, Value)],
    caller: &[String],
) -> Result<IdentityLists> {
    for field in [ALIASES_FIELD, ALIAS_KEYS_FIELD] {
        if props.iter().any(|(f, _)| f == field) {
            return Err(GraphError::IngestError {
                detail: format!(
                    "'{field}' is maintained by the store; pass aliases as the \
                     'aliases' argument instead of a property"
                ),
            });
        }
    }
    let old_name = db.get_prop(key, crate::memory_schema::NAME_FIELD);
    let new_name = props
        .iter()
        .find(|(f, _)| f == crate::memory_schema::NAME_FIELD)
        .map(|(_, v)| v.clone())
        .or_else(|| old_name.clone());
    let text = |v: &Option<Value>| match v {
        Some(Value::Str(s)) => Some(s.clone()),
        _ => None,
    };
    identity_lists(
        db,
        key,
        text(&old_name).as_deref(),
        text(&new_name).as_deref(),
        caller,
    )
}

/// The `aliases` list `key` should carry after a write that sets `props` on
/// it, or `None` when that is exactly what it already carries.
///
/// Reads the store, so call it before a batch takes `db` mutably.
pub fn aliases_after_write<F: Fs>(
    db: &GraphDb<F>,
    key: &str,
    props: &[(String, Value)],
    caller: &[String],
) -> Result<Option<Vec<String>>> {
    Ok(identity_lists_after_write(db, key, props, caller)?.aliases)
}

/// The `alias_keys` list `key` should carry after a write that declares
/// `caller`, or `None` when that is exactly what it already carries — which
/// includes a node that holds none and declares none: the property is never
/// written empty.
///
/// Reads the store, so call it before a batch takes `db` mutably.
pub fn alias_keys_after_write<F: Fs>(
    db: &GraphDb<F>,
    key: &str,
    props: &[(String, Value)],
    caller: &[String],
) -> Result<Option<Vec<String>>> {
    Ok(identity_lists_after_write(db, key, props, caller)?.alias_keys)
}

fn identity_props(lists: IdentityLists) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    if let Some(list) = lists.aliases {
        out.push((ALIASES_FIELD.to_string(), aliases_value(&list)));
    }
    if let Some(list) = lists.alias_keys {
        out.push((ALIAS_KEYS_FIELD.to_string(), aliases_value(&list)));
    }
    out
}

/// The store-maintained identity properties a write to `key` must set:
/// `aliases` and `alias_keys`, each only when it changes.
///
/// One call so the two lists are always decided together — an item leaves
/// `aliases` only by entering `alias_keys` in the same write — and so a
/// refusal of either (a property of that name, a foreign value, a cap) happens
/// before anything is written. Reads the store, so call it before a batch
/// takes `db` mutably.
pub fn identity_props_after_write<F: Fs>(
    db: &GraphDb<F>,
    key: &str,
    props: &[(String, Value)],
    caller: &[String],
) -> Result<Vec<(String, Value)>> {
    identity_lists_after_write(db, key, props, caller).map(identity_props)
}

/// The identity properties to set on `key` in the write that removes its
/// `name`: `aliases` as the key alone implies it.
///
/// The name being removed is the old name here, so its words are recognised
/// as derived and dropped rather than kept as declared. Empty for a node that
/// carries no `aliases` list — one the memory tools never described is not
/// given one by a `forget`.
pub fn identity_props_after_forgetting_name<F: Fs>(
    db: &GraphDb<F>,
    key: &str,
) -> Result<Vec<(String, Value)>> {
    if db.get_prop(key, ALIASES_FIELD).is_none() {
        return Ok(Vec::new());
    }
    let old_name = match db.get_prop(key, crate::memory_schema::NAME_FIELD) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    };
    identity_lists(db, key, old_name.as_deref(), None, &[]).map(identity_props)
}

/// Labels whose nodes carry an `aliases` list: the entity labels and the
/// provisional label.
fn identity_labels() -> impl Iterator<Item = &'static str> {
    use crate::memory_schema::{MEMORY_ENTITY_LABELS, PROVISIONAL_LABEL};
    MEMORY_ENTITY_LABELS
        .iter()
        .copied()
        .chain(std::iter::once(PROVISIONAL_LABEL))
}

/// Nodes under an entity label or the provisional label — the nodes the
/// identity preset's rules compare.
pub fn entity_node_count<F: Fs>(db: &GraphDb<F>) -> usize {
    identity_labels()
        .map(|l| db.nodes_with_label(l).len())
        .sum()
}

/// One node the identity backfill writes to: its key, and the identity
/// properties to set on it.
#[derive(Debug, Clone, PartialEq)]
pub struct IdentityBackfill {
    pub key: String,
    pub props: Vec<(String, Value)>,
}

impl IdentityBackfill {
    /// Whether this node's backfill sets `field`.
    #[must_use]
    pub fn sets(&self, field: &str) -> bool {
        self.props.iter().any(|(f, _)| f == field)
    }
}

/// Every node under an entity label or the provisional label whose stored
/// identity lists differ from what its key, its name and its own data imply,
/// with the properties to set. Key order.
///
/// For most nodes that is one property: an `aliases` list where there was
/// none. A node that carries items in `aliases` its key and name do not imply
/// — a user's own, or aliases declared before they left that list — has them
/// moved to `alias_keys` here too, so applying the preset drops nothing.
pub fn aliases_to_backfill<F: Fs>(db: &GraphDb<F>) -> Result<Vec<IdentityBackfill>> {
    let mut keys: Vec<String> = identity_labels()
        .flat_map(|l| db.nodes_with_label(l))
        .map(|n| n.key().to_string())
        .collect();
    keys.sort();
    let mut out = Vec::new();
    for key in keys {
        let props = identity_props_after_write(db, &key, &[], &[])?;
        if !props.is_empty() {
            out.push(IdentityBackfill { key, props });
        }
    }
    Ok(out)
}

/// Write the properties [`aliases_to_backfill`] found, in one commit.
pub fn write_aliases<F: Fs>(db: &mut GraphDb<F>, lists: &[IdentityBackfill]) -> Result<()> {
    if lists.is_empty() {
        return Ok(());
    }
    let mut batch = db.batch();
    for node in lists {
        for (field, value) in &node.props {
            batch.set_prop(&node.key, field, value.clone());
        }
    }
    batch.commit().map(|_| ())
}

/// One resolved identity: nodes every pair of which is linked by `SAME_AS`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct IdentityCluster {
    /// The oldest member — the lowest live dense id.
    pub canonical: String,
    /// Every member, oldest first; `members[0] == canonical`.
    pub members: Vec<String>,
    /// The lowest pairwise score inside the cluster.
    pub weakest: f64,
}

/// [`identity_clusters`]' answer.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct IdentityReport {
    /// Largest first, then oldest canonical first.
    pub clusters: Vec<IdentityCluster>,
    /// Nodes with at least one `SAME_AS` claim at or above the floor.
    pub linked: usize,
    /// Distinct unordered pairs at or above the floor.
    pub claims: usize,
    /// The floor the claims were read at.
    pub floor: f64,
}

/// Resolve `SAME_AS` claims into identities, by complete linkage.
///
/// A cluster is a set in which **every** pair is linked at `floor` or above.
/// Built greedily and deterministically: seeds in ascending dense id, and for
/// each seed its neighbours in ascending dense id, each admitted only if it is
/// linked to every member admitted so far. A node goes into the first cluster
/// that admits it and no other.
///
/// Three properties follow, and each is the reason for the choice:
///
/// - **No daisy chain.** `a~b` and `b~c` without `a~c` is two claims, not one
///   person, so `c` is not pulled in. Transitive closure would merge them;
///   so does modularity clustering once the rest of the graph is large
///   (`communities` put a six-node chain into one community beside 300
///   unrelated pairs, and split it when alone).
/// - **Locality.** A cluster depends only on the nodes linked to its members.
///   Adding a node with no claim to them cannot move it.
/// - **A stable canonical.** Ids are never reused and a new node always gets
///   a higher one, so a later arrival can join a cluster but never displace
///   its canonical — the oldest node, as settled in the spec's O-2.
///
/// A singleton is not a cluster and is not returned. Reads every `SAME_AS`
/// edge once ([`GraphDb::weighted_edges`]); an edge with no score is a
/// caller's own assertion and counts as 1.0, and so does one whose `weight`
/// is not a number (`weighted_edges` reads only `Int` and `Float`) — the same
/// rule [`same_as_pairs`] applies.
pub fn identity_clusters<F: Fs>(db: &GraphDb<F>, floor: f64) -> IdentityReport {
    use std::collections::BTreeMap;
    let mut key_of: BTreeMap<u32, String> = BTreeMap::new();
    let mut adj: BTreeMap<u32, BTreeMap<u32, f64>> = BTreeMap::new();
    for (src, dst, weight) in db.weighted_edges(SAME_AS_EDGE, Some(SAME_AS_WEIGHT)) {
        let score = weight.unwrap_or(1.0);
        if score < floor || src == dst {
            continue;
        }
        let (Some(s), Some(d)) = (db.dense_id(&src), db.dense_id(&dst)) else {
            continue;
        };
        key_of.insert(s, src);
        key_of.insert(d, dst);
        for (x, y) in [(s, d), (d, s)] {
            let slot = adj.entry(x).or_default().entry(y).or_insert(score);
            if score > *slot {
                *slot = score;
            }
        }
    }
    let claims = adj.values().map(BTreeMap::len).sum::<usize>() / 2;
    let mut assigned: BTreeSet<u32> = BTreeSet::new();
    let mut clusters: Vec<(u32, Vec<u32>, f64)> = Vec::new();
    for (&seed, neighbours) in &adj {
        if !assigned.insert(seed) {
            continue;
        }
        let mut members = vec![seed];
        let mut weakest = f64::INFINITY;
        for &cand in neighbours.keys() {
            if assigned.contains(&cand) {
                continue;
            }
            let scores: Option<Vec<f64>> = members
                .iter()
                .map(|m| adj.get(m).and_then(|n| n.get(&cand)).copied())
                .collect();
            if let Some(scores) = scores {
                weakest = scores.into_iter().fold(weakest, f64::min);
                members.push(cand);
            }
        }
        if members.len() > 1 {
            assigned.extend(members.iter().copied());
            clusters.push((seed, members, weakest));
        }
    }
    clusters.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    IdentityReport {
        clusters: clusters
            .into_iter()
            .map(|(seed, members, weakest)| IdentityCluster {
                canonical: key_of[&seed].clone(),
                members: members.iter().map(|m| key_of[m].clone()).collect(),
                weakest,
            })
            .collect(),
        linked: adj.len(),
        claims,
        floor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_is_idempotent() {
        for s in [
            "Matthew  Sherlin",
            "matthew_sherlin",
            "JOSÉ-Ñúñez",
            "E\u{301}mile Zola",
            "x\u{301}y",
            "İstanbul",
            "ǅemal",
            // Lowercasing `ᾼ` leaves `ᾳ` beside the accent, which composes
            // to `ᾴ` — only a second NFC pass makes this one stable.
            "ᾼ\u{301}",
            "",
            "!!",
            "a1 B2",
        ] {
            assert_eq!(canonical(&canonical(s)), canonical(s), "{s:?}");
        }
    }

    #[test]
    fn derive_is_deterministic_sorted_and_deduplicated() {
        let once = derive_aliases("Sherlin", Some("Matthew  SHERLIN"));
        assert_eq!(once, ["matthew", "matthew sherlin", "sherlin"]);
        assert_eq!(once, derive_aliases("Sherlin", Some("Matthew  SHERLIN")));
    }
}
