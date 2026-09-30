//! `remember` — write a `Note` the graph can later `recall`.
//!
//! Unlike the code-graph `remember` this module superseded, deleted in 0.7 with
//! the rest of that module, an `about` key here is not required to already
//! exist: a fact may name its subject before anything has described that
//! subject. A missing key is stubbed as a provisional entity
//! (`memory_schema::PROVISIONAL_LABEL`, `memory_schema::PROVISIONAL_PROP`)
//! rather than refusing the whole call — the store learns the shape of what it
//! does not yet know. `facts[]` endpoints get the identical stub, not a bare
//! `insert_edge_upsert` auto-create: an unnamed, unmarked node from a typo'd
//! `object` would be unfindable by the same `provisional` query that surfaces
//! every other guess this module makes (fix round 1, 0.7).
//!
//! Everything one call writes — the entities the caller recognised, the note,
//! the `ABOUT` edges linking the note to `about`, and the facts among those
//! entities — goes through one [`GraphDb::batch`] commit, in that order: an
//! `about` key or a fact endpoint that is also named in `entities` (or, for a
//! fact endpoint, also in `about`) is created as the real entity the caller
//! described, not a provisional stub, because the entity ops are queued (and
//! their keys become visible to the batch's own validation) before the
//! `about` keys and the facts' own endpoints are resolved.

use crate::memory_schema::{NAME_FIELD, PROVISIONAL_LABEL, PROVISIONAL_PROP};
use crate::GraphDb;
use core_storage::fs::Fs;
use core_storage::{GraphError, Result, Value};
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Text length bounds, in characters, after trimming.
const MIN_TEXT_CHARS: usize = 1;
const MAX_TEXT_CHARS: usize = 4000;

/// The `kind` values a `Note` may carry.
pub const NOTE_KINDS: [&str; 3] = ["note", "decision", "todo"];

/// The edge type linking a `Note` to what it is about.
const ABOUT_EDGE: &str = "ABOUT";

/// The `source` a note is stamped with when the caller supplies none.
const DEFAULT_SOURCE: &str = "agent";

/// Ceiling on the store's total declared full-text surface — the defaults
/// `memory_defaults()` ships (8: the five entity labels, `Note.text`,
/// `Concept.summary`, `Entity.name`) plus whatever `remember` self-declares
/// for an `entities[].label` outside those. See the self-declare comment in
/// [`remember`] for why this is bounded rather than unlimited.
const MAX_FULLTEXT_PAIRS: usize = 32;

/// Ceiling on provisional stubs one `remember` call may create — `about`
/// keys and `facts[]` endpoints combined, counted in resolution order
/// (`about` before `facts`). Spec §3.2's own guard: "a cap per commit, so a
/// malformed batch cannot flood the graph."
///
/// A real extraction from one conversation turn names at most a handful of
/// unknown subjects — `about` typically carries one to a few keys, and
/// `facts[]` rarely introduces more than a couple more that `entities`
/// didn't already cover — so this is generous against that shape and tight
/// against the shape of a bug: a loop that built `facts` from a cross
/// product, or a caller that fed a whole document's worth of names into
/// `about` at once. 20 is comfortably above any single legitimate call and
/// well short of "the graph is now full of typos."
///
/// Past the cap, the note and everything else valid in the call still
/// commits — a caller's one bad key should not cost the whole write, the
/// same reasoning that makes an unknown key provisional instead of a refusal
/// in the first place — but a key that would have been stubbed is not: no
/// node is created for it, and no edge (`ABOUT` or a fact's own) names it
/// either, since that edge's endpoint would not exist. It is listed in
/// [`RememberReport::provisional_capped`], so the caller is told plainly
/// rather than discovering a silently short digest later — the same
/// "unmistakable, not a quiet field" standard the label-mismatch fix in
/// `upsert_entity` was held to.
const MAX_PROVISIONAL_PER_COMMIT: usize = 20;

/// One entity the caller recognised in the text.
///
/// Create-or-update, same as [`describe_entity`]: a `key` already in the
/// store is described (its `props` set, its provisional mark cleared if it
/// had one) rather than re-created.
#[derive(Debug, Clone)]
pub struct EntityIn {
    pub key: String,
    pub label: String,
    pub props: BTreeMap<String, Value>,
}

/// One relationship the caller recognised, between keys named in `entities`
/// or `about`.
///
/// `subject`/`object` need not already exist — same provisional treatment as
/// an unknown `about` key — so a fact can arrive before either endpoint has
/// been described.
#[derive(Debug, Clone)]
pub struct FactIn {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

/// What to remember.
pub struct RememberInput<'a> {
    /// The note's text, trimmed to [`MIN_TEXT_CHARS`]..=[`MAX_TEXT_CHARS`]
    /// characters.
    pub text: &'a str,
    /// Keys the note is about. A key that does not exist yet is created as a
    /// provisional entity rather than refusing the call.
    pub about: &'a [String],
    /// One of [`NOTE_KINDS`].
    pub kind: &'a str,
    /// Unix seconds the note was written at. Part of the note's key, so
    /// remembering the same text again at the same `ts` is a no-op rather
    /// than a duplicate. Caller-supplied so a fact imported from a
    /// transcript is stamped with when it was said, not when it was
    /// imported.
    pub ts: i64,
    /// Where this came from — a session id, a file, a person. `None`
    /// defaults to `"agent"`, the only value this ever stored before 0.7.
    pub source: Option<&'a str>,
    /// Entities the caller recognised in the text, written in the same
    /// commit as the note.
    pub entities: &'a [EntityIn],
    /// Relationships the caller recognised, written in the same commit as
    /// the note.
    pub facts: &'a [FactIn],
}

/// Everything one `remember` call writes.
#[derive(Debug, Default)]
pub struct RememberReport {
    /// The note's key.
    pub note: String,
    /// Entities this call brought into existence.
    pub created: usize,
    /// `about` keys and entities that already existed.
    pub matched: usize,
    /// Derived edges the rules produced for this commit, incident on the
    /// *note* — edges the engine's rule provenance attributes to a rule,
    /// never one this call inserted itself (`ABOUT`, a fact's own edge, or a
    /// provisional stub have no rule and never count here). `memory_defaults()`
    /// ships with zero rules (`SAME_AS` arrives in a later plan), so this is
    /// `0` against a plain memory store today — and it stays structurally `0`
    /// for the foreseeable future even once `SAME_AS` exists: that rule is
    /// Person→Person, never incident on a note, so nothing it derives is
    /// counted here. This field only moves once a rule exists whose
    /// predicate can match the note itself — such as an `about_<label>` rule
    /// `ingest-git` declared before 0.7, which derives the note's `ABOUT`
    /// edge (counted here, on the commit that derived it) in place of the
    /// insert this call would otherwise make.
    pub derived: usize,
    /// `about` keys and fact endpoints that had to be stubbed, in the order
    /// each was first seen (`about` before `facts`).
    pub provisional: Vec<String>,
    /// `about` keys and fact endpoints that would have been stubbed but were
    /// refused instead — [`MAX_PROVISIONAL_PER_COMMIT`] was already spent by
    /// the time this call reached them. Neither the node nor any edge naming
    /// it exists; everything else in the call still committed. Empty on
    /// every call this cap does not bind.
    pub provisional_capped: Vec<String>,
    /// Entity labels full-text search had never seen before this call, now
    /// declared on `(label, "name")` so `recall` can reach them — the same
    /// self-declare `Note.text` gets, extended to `entities[].label`. Empty
    /// on every call that named no unseen label, or that found
    /// [`MAX_FULLTEXT_PAIRS`] already spent.
    pub fulltext_declared: Vec<String>,
}

/// Create-or-update one entity, clearing any provisional mark.
///
/// Upsert semantics lived only in `tool_upsert_entity` before 0.7, so HTTP
/// and Python had no upsert and could not clear a provisional mark. This is
/// the one implementation **`tool_upsert_entity` (the MCP surface) calls** —
/// HTTP and Python are untouched by this branch and still have no upsert of
/// their own at all. Binding parity, so a later plan's HTTP/Python surface
/// calls this same function instead of growing a third copy, is future work.
///
/// `label` is used only when `key` does not already exist — an update never
/// changes a node's label. A subject stops being provisional the moment
/// anything describes it, whoever does the describing, so the clearing lives
/// here rather than in any one caller: `remove_prop` is `Ok(false)` and logs
/// nothing when the field is already absent, so this is unconditional and
/// free on a node that was never provisional.
///
/// A create passes `props` straight to [`GraphDb::insert_node`] rather than
/// inserting bare and following up with [`GraphDb::set_props`]: a namespace
/// is set at insert and immutable after, so a caller-supplied `ns` reaching
/// `set_props` on a node that already exists (even one this same call just
/// created) is the engine's `NamespaceImmutable` refusal rather than the
/// no-op it should be.
pub fn describe_entity<F: Fs>(
    db: &mut GraphDb<F>,
    key: &str,
    label: Option<&str>,
    props: &[(String, Value)],
) -> Result<bool> {
    if db.has_node(key) {
        if !props.is_empty() {
            db.set_props(key, props.to_vec())?;
        }
        let _ = db.remove_prop(key, PROVISIONAL_PROP)?;
        Ok(false)
    } else {
        db.insert_node(label.unwrap_or(PROVISIONAL_LABEL), key, props.to_vec())?;
        let _ = db.remove_prop(key, PROVISIONAL_PROP)?;
        Ok(true)
    }
}

/// Write `input` as a `Note`, describing whatever it names along the way.
///
/// Validated before anything is written: `text` must be
/// [`MIN_TEXT_CHARS`]..=[`MAX_TEXT_CHARS`] characters after trimming, and
/// `kind` must be one of [`NOTE_KINDS`]. Unlike the code-graph `remember`
/// 0.7 deleted, an unknown `about` key is never an error — see the module
/// docs.
///
/// The note's key is `"note:"` followed by 16 hex characters of a stable
/// 64-bit hash of `ts` and `text` (see [`note_key`]), so remembering the
/// same text at the same `ts` again returns the same key without writing a
/// second node.
///
/// Also ensures full-text search is enabled on `Note.text`, so a store whose
/// very first write is a `remember` call can still be recalled from.
pub fn remember<F: Fs>(db: &mut GraphDb<F>, input: &RememberInput<'_>) -> Result<RememberReport> {
    let text = input.text.trim();
    let len = text.chars().count();
    if !(MIN_TEXT_CHARS..=MAX_TEXT_CHARS).contains(&len) {
        return Err(GraphError::IngestError {
            detail: format!(
                "remember: text must be {MIN_TEXT_CHARS}..={MAX_TEXT_CHARS} characters \
                 after trimming, got {len}"
            ),
        });
    }
    if !NOTE_KINDS.contains(&input.kind) {
        return Err(GraphError::IngestError {
            detail: format!(
                "remember: kind must be one of {}, got {:?}",
                NOTE_KINDS.join(", "),
                input.kind
            ),
        });
    }

    let mut fulltext = db.fulltext_pairs();
    if !fulltext.contains(&("Note".to_string(), "text".to_string())) {
        db.enable_fulltext("Note", "text")?;
        fulltext.push(("Note".to_string(), "text".to_string()));
    }

    // Self-declare full-text for an `entities[].label` full-text has never
    // seen — the same fix `Note.text` gets above, extended to entities.
    // `memory_defaults()` declares `(label, "name")` for exactly the five
    // built-in labels (`Person`, `Org`, `Project`, `Concept`, `Event`) plus
    // the provisional label; `entities[].label` is free-form (the spec's own
    // worked example, `{key:"v0.7", label:"Release"}`, uses a sixth), so an
    // entity under any other label landed with a `name` no `recall` could
    // ever reach — spec §1.1(b)'s defect, reproduced through this release's
    // own new feature (fix round 2, 0.7).
    //
    // Bounded by `MAX_FULLTEXT_PAIRS`: `enable_fulltext`'s declaration is
    // rebuilt from scratch on every re-open (227 ms measured against 3.8 ms
    // with none, at a small declared surface — `memory_schema`'s module
    // doc), and `entities[].label` is a caller-supplied string with no
    // schema behind it — a well-behaved caller uses a handful of distinct
    // labels, but nothing stops a run of calls from feeding a fresh one each
    // time and growing the declared surface, and every future open's cost,
    // without bound. Past the cap a new label's entity is still written and
    // reported exactly as any other — only made not full-text-searchable
    // yet, a bounded-cost degradation rather than a refusal.
    let mut fulltext_declared: Vec<String> = Vec::new();
    {
        let mut declared_this_call: BTreeSet<&str> = BTreeSet::new();
        for entity in input.entities {
            let label = entity.label.as_str();
            if !declared_this_call.insert(label) {
                continue;
            }
            let pair = (label.to_string(), NAME_FIELD.to_string());
            if fulltext.contains(&pair) || fulltext.len() >= MAX_FULLTEXT_PAIRS {
                continue;
            }
            db.enable_fulltext(label, NAME_FIELD)?;
            fulltext.push(pair);
            fulltext_declared.push(label.to_string());
        }
    }

    let source = input.source.unwrap_or(DEFAULT_SOURCE).to_string();
    let key = note_key(input.ts, text);

    // Deduped, first-occurrence order kept: that is the order
    // `RememberReport::provisional` promises the caller.
    let mut about: Vec<String> = Vec::with_capacity(input.about.len());
    {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for k in input.about {
            if seen.insert(k.as_str()) {
                about.push(k.clone());
            }
        }
    }
    let entity_keys: BTreeSet<&str> = input.entities.iter().map(|e| e.key.as_str()).collect();

    // Every existence check happens before `db.batch()` takes `db` mutably
    // below — batch validation sees only ops queued earlier in the same
    // frame, not the live store, so "does this already exist" has to be
    // asked of the store now.
    let entity_existed: Vec<bool> = input.entities.iter().map(|e| db.has_node(&e.key)).collect();
    let about_existed: Vec<Option<bool>> = about
        .iter()
        .map(|k| {
            if entity_keys.contains(k.as_str()) {
                // Resolved by the `entities` loop below instead.
                None
            } else {
                Some(db.has_node(k))
            }
        })
        .collect();
    let note_existed = db.has_node(&key);
    // The note's edges as they stand, for two answers below: which `about`
    // links already exist (step 4), and how many derived edges were there
    // before this commit (so `derived` counts only what it produced).
    let (already_about, derived_before): (BTreeSet<String>, usize) = if note_existed {
        let edges = db.node_edges(&key)?;
        (
            edges
                .iter()
                .filter(|e| e.edge_type == ABOUT_EDGE && e.src_key == key)
                .map(|e| e.dst_key.clone())
                .collect(),
            edges.iter().filter(|e| e.derived).count(),
        )
    } else {
        (BTreeSet::new(), 0)
    };

    // Facts' endpoints not already named in `entities` or `about`: existence
    // is checked now, before the batch, same as above. One not seen anywhere
    // gets exactly the `about` path's provisional treatment — a stub with a
    // `name` and `provisional: true`, reported in `provisional` — rather than
    // the bare, unmarked node `insert_edge_upsert`'s own auto-create would
    // otherwise leave behind with no signal anywhere that it was guessed.
    let known: BTreeSet<&str> = entity_keys
        .iter()
        .copied()
        .chain(about.iter().map(String::as_str))
        .collect();
    let mut fact_endpoints: Vec<&str> = Vec::new();
    {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for fact in input.facts {
            for k in [fact.subject.as_str(), fact.object.as_str()] {
                if !known.contains(k) && seen.insert(k) {
                    fact_endpoints.push(k);
                }
            }
        }
    }
    let fact_endpoint_existed: Vec<bool> = fact_endpoints.iter().map(|k| db.has_node(k)).collect();

    let mut report = RememberReport {
        note: key.clone(),
        fulltext_declared,
        ..Default::default()
    };

    let mut batch = db.batch();

    // Shared across steps 2 and 5: one `MAX_PROVISIONAL_PER_COMMIT` budget
    // for `about` and `facts[]` combined, `about` spent first. `capped`
    // collects a key the budget ran out on so steps 4 and 6 can skip the
    // edge that would otherwise name a node this call never created.
    let mut provisional_count = 0usize;
    let mut capped: BTreeSet<String> = BTreeSet::new();

    // 1. Entities first, so an `about` key also named here lands as the real
    //    entity rather than a provisional stub.
    for (entity, existed) in input.entities.iter().zip(&entity_existed) {
        if *existed {
            for (field, value) in &entity.props {
                batch.set_prop(&entity.key, field, value.clone());
            }
            report.matched += 1;
        } else {
            // Straight to `insert_node`, not an empty insert followed by
            // `set_props` — see `describe_entity`'s doc comment for why an
            // insert-time-only field (a namespace) would otherwise be
            // refused rather than accepted.
            let props: Vec<(String, Value)> = entity
                .props
                .iter()
                .map(|(f, v)| (f.clone(), v.clone()))
                .collect();
            batch.insert_node(&entity.label, &entity.key, props);
            report.created += 1;
        }
        batch.remove_prop(&entity.key, PROVISIONAL_PROP);
    }

    // 2. Provisional stubs for `about` keys `entities` did not already cover
    //    — bounded by `MAX_PROVISIONAL_PER_COMMIT` (see its doc comment).
    for (k, status) in about.iter().zip(&about_existed) {
        match status {
            None => {}
            Some(true) => report.matched += 1,
            Some(false) => {
                if provisional_count < MAX_PROVISIONAL_PER_COMMIT {
                    batch.insert_node(
                        PROVISIONAL_LABEL,
                        k,
                        vec![
                            (NAME_FIELD.to_string(), Value::Str(k.clone())),
                            (PROVISIONAL_PROP.to_string(), Value::Bool(true)),
                        ],
                    );
                    report.provisional.push(k.clone());
                    provisional_count += 1;
                } else {
                    report.provisional_capped.push(k.clone());
                    capped.insert(k.clone());
                }
            }
        }
    }

    // 3. The note. `about` here is the caller's full, literal claim — a key
    //    the cap refused stays listed, even though its edge (step 4) is not
    //    written: the note is a record of what it was told, not only of what
    //    could be linked, matching this store's audit-trail default (spec
    //    §3.2, O-4). `report.provisional_capped` is the place that tells the
    //    caller which of these has no edge.
    if !note_existed {
        let mut props: Vec<(String, Value)> = vec![
            ("id".into(), Value::Str(key.clone())),
            ("text".into(), Value::Str(text.to_string())),
            ("kind".into(), Value::Str(input.kind.to_string())),
            ("ts".into(), Value::Int(input.ts)),
            ("source".into(), Value::Str(source)),
        ];
        if !about.is_empty() {
            props.push((
                "about".into(),
                Value::List(about.iter().cloned().map(Value::Str).collect()),
            ));
        }
        batch.insert_node("Note", &key, props);
    }

    // 4. ABOUT edges, note -> each about key that actually exists. A key the
    //    cap refused (step 2) is skipped — its node was never created, so
    //    the edge can't be either — and a link that already exists is left
    //    alone. That covers the duplicate (the note already existed and
    //    named the same `about` before) and, on a store carrying a 0.6
    //    `about_*` rule, an edge that rule derived and owns: inserting it
    //    again would be refused as a write to a rule-owned edge.
    for k in &about {
        if capped.contains(k) || already_about.contains(k) {
            continue;
        }
        batch.insert_edge(ABOUT_EDGE, &key, k);
    }

    // 5. Provisional stubs for fact endpoints `entities`/`about` did not
    //    already cover — same shape as step 2, same shared
    //    `MAX_PROVISIONAL_PER_COMMIT` budget (`about` already spent its
    //    share above), so a typo'd `facts[].object` is as visible and as
    //    findable as an unknown `about` key, not a nameless, unmarked node
    //    with no report entry anywhere — up to the same per-commit cap.
    for (k, existed) in fact_endpoints.iter().zip(&fact_endpoint_existed) {
        if *existed {
            continue;
        }
        if provisional_count < MAX_PROVISIONAL_PER_COMMIT {
            batch.insert_node(
                PROVISIONAL_LABEL,
                k,
                vec![
                    (NAME_FIELD.to_string(), Value::Str((*k).to_string())),
                    (PROVISIONAL_PROP.to_string(), Value::Bool(true)),
                ],
            );
            report.provisional.push((*k).to_string());
            provisional_count += 1;
        } else {
            report.provisional_capped.push((*k).to_string());
            capped.insert((*k).to_string());
        }
    }

    // 6. Facts whose endpoints all exist — described above, already in the
    //    store, or just stubbed. A fact naming an endpoint the cap refused
    //    (step 5) is skipped whole: that endpoint does not exist, so the
    //    edge can't either.
    for fact in input.facts {
        if capped.contains(&fact.subject) || capped.contains(&fact.object) {
            continue;
        }
        batch.insert_edge(&fact.predicate, &fact.subject, &fact.object);
    }

    batch.commit()?;

    // `commit()`'s own (nodes, edges) counts are the ops this call queued,
    // not what any live rule derived from them — read the note's edges back
    // and ask the engine which ones it owns.
    let derived_after = db
        .node_edges(&key)?
        .into_iter()
        .filter(|e| e.derived)
        .count();
    report.derived = derived_after.saturating_sub(derived_before);

    Ok(report)
}

/// The key one `remember` call writes to: `"note:"` followed by 16 hex
/// characters of a 64-bit FNV-1a hash of `ts` and `text`.
///
/// Same construction as the code-graph `note_key` 0.7 deleted, so a note
/// written by 0.6 and the same `ts` and `text` remembered by 0.7 share one
/// key. FNV-1a rather than `blake3` for the
/// same dependency reason: `blake3` is confined to `crates/code-extract`,
/// which `core-api` cannot depend on.
fn note_key(ts: i64, text: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for b in ts.to_string().bytes().chain(text.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("note:{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_a_function_of_ts_and_text_alone() {
        assert_eq!(note_key(1, "a"), note_key(1, "a"));
        assert_ne!(note_key(1, "a"), note_key(2, "a"));
        assert_ne!(note_key(1, "a"), note_key(1, "b"));
        assert!(note_key(1, "a").strip_prefix("note:").unwrap().len() == 16);
    }
}
