//! `remember` — write a `Note` the graph can later `recall`.
//!
//! Unlike [`repograph::remember`](crate::repograph::remember::remember),
//! which this module supersedes for the memory surface, an `about` key here
//! is not required to already exist: a fact may name its subject before
//! anything has described that subject. A missing key is stubbed as a
//! provisional entity (`memory_schema::PROVISIONAL_LABEL`,
//! `memory_schema::PROVISIONAL_PROP`) rather than refusing the whole call —
//! the store learns the shape of what it does not yet know.
//!
//! Everything one call writes — the entities the caller recognised, the note,
//! the `ABOUT` edges linking the note to `about`, and the facts among those
//! entities — goes through one [`GraphDb::batch`] commit, in that order: an
//! `about` key that is also named in `entities` is created as the real
//! entity the caller described, not a provisional stub, because the entity
//! ops are queued (and their keys become visible to the batch's own
//! validation) before the `about` keys are resolved.
//!
//! Deliberately independent of `repograph` — no import from it — the same
//! choice [`super::recall`] makes, so `repograph` can be deleted whole once
//! the plan that replaces it lands.

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
    /// Derived edges the rules produced for this commit.
    pub derived: usize,
    /// `about` keys that had to be stubbed, in the order given.
    pub provisional: Vec<String>,
}

/// Create-or-update one entity, clearing any provisional mark.
///
/// Upsert semantics lived only in `tool_upsert_entity` before 0.7, so HTTP
/// and Python had no upsert and could not clear a provisional mark. One
/// implementation, every surface.
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
/// `kind` must be one of [`NOTE_KINDS`]. Unlike
/// [`repograph::remember`](crate::repograph::remember::remember), an
/// unknown `about` key is never an error — see the module docs.
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

    if !db
        .fulltext_pairs()
        .contains(&("Note".to_string(), "text".to_string()))
    {
        db.enable_fulltext("Note", "text")?;
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

    let mut report = RememberReport {
        note: key.clone(),
        ..Default::default()
    };

    let mut batch = db.batch();

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

    // 2. Provisional stubs for `about` keys `entities` did not already cover.
    for (k, status) in about.iter().zip(&about_existed) {
        match status {
            None => {}
            Some(true) => report.matched += 1,
            Some(false) => {
                batch.insert_node(
                    PROVISIONAL_LABEL,
                    k,
                    vec![
                        (NAME_FIELD.to_string(), Value::Str(k.clone())),
                        (PROVISIONAL_PROP.to_string(), Value::Bool(true)),
                    ],
                );
                report.provisional.push(k.clone());
            }
        }
    }

    // 3. The note.
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

    // 4. ABOUT edges, note -> each about key. A duplicate (the note already
    //    existed and named the same `about` before) is a silent no-op.
    for k in &about {
        batch.insert_edge(ABOUT_EDGE, &key, k);
    }

    // 5. Facts. An endpoint not already named above is created provisional
    //    rather than failing the whole commit — the same "a fact may arrive
    //    before its subject" treatment `about` gets.
    for fact in input.facts {
        batch.insert_edge_upsert(
            &fact.predicate,
            &fact.subject,
            &fact.object,
            PROVISIONAL_LABEL,
        );
    }

    batch.commit()?;

    // `commit()`'s own (nodes, edges) counts are the ops this call queued,
    // not what any live rule derived from them — read the note's edges back
    // and ask the engine which ones it owns.
    report.derived = db
        .node_edges(&key)?
        .into_iter()
        .filter(|e| e.derived)
        .count();

    Ok(report)
}

/// The key one `remember` call writes to: `"note:"` followed by 16 hex
/// characters of a 64-bit FNV-1a hash of `ts` and `text`.
///
/// Same construction as
/// [`repograph::remember::note_key`](crate::repograph::remember), duplicated
/// rather than imported — see the module docs — so the two are byte-for-byte
/// identical rather than merely similar. FNV-1a rather than `blake3` for the
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
