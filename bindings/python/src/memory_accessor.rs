//! `db.memory`: the agent-memory methods, under their own namespace.
//!
//! A plain `GraphDb` is a general-purpose graph store; the methods here are the
//! ones that write and read notes, entities and identities the way the memory
//! tools do. The accessor holds the store's shared state and the handle's scope
//! directly, not a reference to the `GraphDb` object it came from, so it forms
//! no reference cycle: dropping the handle releases the store.

use super::*;

/// The memory-API methods of one handle: what `db.memory` returns.
///
/// It shares the handle's store and carries its scope, so a `scoped()` handle's
/// `memory` is scoped the same way and refuses what the handle refuses.
#[pyclass(name = "MemoryAccessor", module = "mushroomdb")]
pub(crate) struct MemoryAccessor {
    pub(crate) db: GraphDb,
}

#[pymethods]
impl MemoryAccessor {
    /// Write a note the store can later `recall`, and whatever it names.
    ///
    /// `about` is the keys the note is about. A key that does not exist is
    /// created as a provisional `Entity` rather than refusing the call, and
    /// listed under `provisional` in the report.
    ///
    /// `entities` is a list of `{"key", "label", "props"?, "aliases"?}`
    /// dicts — entities you recognised in the text, created or updated in the
    /// same commit. `facts` is a list of `{"subject", "predicate", "object"}`
    /// dicts — relationships among them, written as edges. Either may be a
    /// tuple or any other sequence, as `about` may.
    ///
    /// `kind` is `"note"` (the default), `"decision"` or `"todo"`. `ts` is
    /// Unix seconds and defaults to now; it is part of the note's key, so the
    /// same text at the same `ts` is the same note. `source` defaults to
    /// `"agent"`.
    ///
    /// Returns the report as a dict: `note` (the note's key — **not** `key`,
    /// which is what the MCP tool's JSON reply calls it), `created`,
    /// `matched`, `derived`, `provisional`, `provisional_capped` (keys past
    /// the per-call cap of 20, **not** created), `fulltext_declared`,
    /// `same_as` and `same_as_lost` (each a list of `{"a", "b", "score"}`:
    /// the identity links this call made, and the ones it retracted), and
    /// `same_as_unread` (`None`, or the error text when the write committed
    /// but its links could not be read back, so both lists are empty because
    /// nothing was read). A `score` is the raw float (`0.6666666666666666`);
    /// the MCP tool prints the same score to two decimals.
    ///
    /// Raises `IngestError` when the text is empty or over 4,000 characters,
    /// the `kind` is unknown, an entity's `props` carry `aliases` or
    /// `alias_keys`, a name is empty, only whitespace or only invisible
    /// characters — a key in
    /// `about`, an entity's `key` or `label`, or a fact's `subject`,
    /// `predicate` or `object`; nothing is written. Its `.detail` is the
    /// bare sentence; the message carries an `ingest error: ` prefix in
    /// front of it.
    ///
    /// A store this binding creates has no memory schema. `remember`
    /// declares full-text on `Note.text` and on each new entity label's
    /// `name` itself, so `recall` works after the first call.
    ///
    /// ```python
    /// r = db.memory.remember("Matthew is driving 0.7", about=["matthew"],
    ///                        entities=[{"key": "v0.7", "label": "Release"}],
    ///                        facts=[{"subject": "matthew", "predicate": "WORKS_ON", "object": "v0.7"}])
    /// r["note"]         # "note:…"
    /// r["provisional"]  # ["matthew"]
    /// ```
    #[allow(clippy::too_many_arguments)]
    #[pyo3(
        signature = (text, about = None, kind = None, ts = None, source = None, entities = None, facts = None),
        text_signature = "($self, text, about=None, kind=None, ts=None, source=None, entities=None, facts=None)"
    )]
    pub(crate) fn remember(
        &self,
        py: Python<'_>,
        text: &str,
        about: Option<Vec<String>>,
        kind: Option<&str>,
        ts: Option<i64>,
        source: Option<&str>,
        entities: Option<Bound<'_, PyAny>>,
        facts: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        use core_api::memory::remember::{
            dedup_keep_order, remember, unix_now_secs, RememberInput, DEFAULT_NOTE_KIND,
        };
        // First, so a scoped handle is refused before any argument is read.
        self.db.refuse_if_scoped()?;
        let mut about = about.unwrap_or_default();
        dedup_keep_order(&mut about);
        let entities = match entities {
            Some(list) => entities_from_py(&list)?,
            None => Vec::new(),
        };
        let facts = match facts {
            Some(list) => facts_from_py(&list)?,
            None => Vec::new(),
        };
        let input = RememberInput {
            text,
            about: &about,
            kind: kind.unwrap_or(DEFAULT_NOTE_KIND),
            ts: ts.unwrap_or_else(unix_now_secs),
            source,
            entities: &entities,
            facts: &facts,
        };
        let report = self.db.with_mut(|db| remember(db, &input))?;
        report_to_py(py, serde_json::to_value(&report))
    }

    /// What the store holds about a topic, as rows.
    ///
    /// Returns `{"indexed": bool, "terms": int, "hits": [...]}`. Each hit is
    /// `{"key", "label", "summary", "covered", "score"}`, best first:
    /// `covered` is how many of the topic's `terms` the node's own text
    /// holds, and it leads the ranking — the fused `score` is nearly flat.
    /// A hit covers at least half the terms. At most six hits. `terms == 0`
    /// with hits present means the topic was all stopwords: its words were
    /// searched together and every hit's `covered` is `0`. `summary` is
    /// the first 120 characters of the node's `text`, `summary` or `name`,
    /// or `None` when it has none of them.
    ///
    /// `indexed` is `False` when the store declares no full-text index at
    /// all, so no topic can match — a different answer from an empty `hits`.
    /// `remember` declares `Note.text` on its first call.
    ///
    /// This is the MCP `recall` tool's ranking, as data; the tool renders the
    /// same rows as a digest, and this method does not offer the digest.
    ///
    /// **The rows are raw stored content.** `key`, `label` and `summary` are
    /// unsanitized: only the digest renderer replaces control characters,
    /// line separators and bidi or zero-width characters with spaces. If you
    /// render a row into an assistant's context, that sanitisation is yours
    /// to do — a stored line break can otherwise forge a line of your
    /// prompt. This binding exposes no helper for it.
    ///
    /// Refused on a `scoped()` handle with `ValueError`.
    #[pyo3(text_signature = "($self, topic)")]
    pub(crate) fn recall(&self, py: Python<'_>, topic: &str) -> PyResult<Py<PyAny>> {
        self.db.refuse_read_if_scoped("recall")?;
        let rows = self
            .db
            .with_ref(|db| Ok(core_api::memory::recall::recall_rows(db, topic)))?;
        report_to_py(py, serde_json::to_value(&rows))
    }

    /// Create or update one entity by key, the way the memory tools do.
    ///
    /// Unlike `upsert_node`, this maintains the entity's two identity lists:
    /// `aliases`, which the store derives from the current key and `name` and
    /// recomputes on every call — never accumulated, never yours to set — and
    /// `alias_keys`, the `aliases` you pass, kept as written and accumulating.
    /// It also sets `id` to the key and clears a `provisional` mark
    /// `remember` left.
    ///
    /// `label` is required to create and optional to update. **An update
    /// never changes a label**: one that differs from the stored label
    /// raises `IngestError` and nothing in `props` is written.
    ///
    /// `namespace` is the namespace a created node lands in. On an existing
    /// node its own namespace is a no-op and another raises
    /// `NamespaceImmutable`. A `props["ns"]` that disagrees with `namespace`
    /// is a `ValueError`.
    ///
    /// Returns `{"key", "label", "created", "updated_fields",
    /// "same_as_lost", "same_as_unread"}`. `same_as_lost` lists the identity
    /// links this update retracted, as `{"a", "b", "score"}`: a changed `name`
    /// can take a full-name link below the floor. `same_as_unread` is `None`,
    /// or the error text when the update committed but the links could not be
    /// read back afterwards, so `same_as_lost` is empty because nothing was
    /// read. A `score` is the raw float (`0.6666666666666666`); the MCP tool
    /// prints it to two decimals.
    ///
    /// `aliases` or `alias_keys` inside `props` raise `IngestError`: the
    /// store maintains both. So does a `key` that is empty or only
    /// whitespace, and a create under such a `label`. An `IngestError`'s
    /// `.detail` is the bare sentence; its message carries an
    /// `ingest error: ` prefix.
    ///
    /// ```python
    /// db.memory.upsert_entity("ada", {"name": "Ada Lovelace"}, label="Person", aliases=["Countess"])
    /// ```
    #[pyo3(
        signature = (key, props, label = None, aliases = None, namespace = None),
        text_signature = "($self, key, props, label=None, aliases=None, namespace=None)"
    )]
    pub(crate) fn upsert_entity(
        &self,
        py: Python<'_>,
        key: &str,
        props: Bound<'_, PyDict>,
        label: Option<&str>,
        aliases: Option<Vec<String>>,
        namespace: Option<&str>,
    ) -> PyResult<Py<PyDict>> {
        // First: the engine call reads the stored label before it writes, and
        // on a scoped handle that read must never happen (defect #8).
        self.db.refuse_if_scoped()?;
        let mut row: BTreeMap<String, Value> = dict_to_props(&props)?.into_iter().collect();
        if let Some(ns) = check_namespace(namespace)? {
            match row.get(NS_PROP).cloned() {
                Some(Value::Str(s)) if s == ns => {}
                Some(existing) => {
                    return Err(PyValueError::new_err(format!(
                        "upsert_entity: props ns is {existing:?} but namespace={ns:?}; a node is \
                         created in one namespace, so pass one or the other"
                    )))
                }
                None => {
                    row.insert(NS_PROP.to_string(), Value::Str(ns.to_string()));
                }
            }
        }
        let aliases = aliases.unwrap_or_default();
        let outcome = self.db.with_mut(|db| {
            core_api::memory::remember::upsert_entity(db, key, label, row, &aliases)
        })?;
        let d = PyDict::new(py);
        d.set_item("key", key)?;
        d.set_item("label", &outcome.label)?;
        d.set_item("created", outcome.created)?;
        d.set_item("updated_fields", outcome.updated_fields)?;
        let lost = PyList::empty(py);
        for pair in &outcome.same_as_lost {
            let pd = PyDict::new(py);
            pd.set_item("a", &pair.a)?;
            pd.set_item("b", &pair.b)?;
            pd.set_item("score", pair.score)?;
            lost.append(pd)?;
        }
        d.set_item("same_as_lost", lost)?;
        d.set_item("same_as_unread", &outcome.same_as_unread)?;
        Ok(d.unbind())
    }

    /// What the store holds and how it is wired.
    ///
    /// Returns a dict: `brief` (`nodes` and `edges` counts, `labels` each
    /// with its fields, `edge_types` each with its endpoints, `commits`,
    /// `roles`, `recipes`, `partial`), `rules` (each with its predicate in
    /// one clause and its namespace), `fulltext` and `indexes` (`[label,
    /// field]` lists), `provisional` (how many nodes `remember` named and
    /// nothing has described) and `provisional_sample` (the first ten keys).
    ///
    /// `budget_ms` bounds the counting; a spent budget sets
    /// `brief["partial"]` and the counts are then lower bounds. **`0` is not
    /// "no limit" here**, as it is for the graph algorithms: it is a budget
    /// already spent, and returns a partial report. For an unhurried report
    /// pass a large value, such as `60_000`.
    ///
    /// The MCP `schema` tool renders this same report. The names under
    /// `rules`, `fulltext`, `indexes` and `provisional_sample` are raw stored
    /// content, unsanitized, as `recall`'s rows are.
    ///
    /// Refused on a `scoped()` handle with `ValueError`.
    #[allow(deprecated)]
    #[pyo3(signature = (budget_ms = 1000), text_signature = "($self, budget_ms=1000)")]
    pub(crate) fn schema_report(&self, py: Python<'_>, budget_ms: u64) -> PyResult<Py<PyAny>> {
        self.db.refuse_read_if_scoped("schema_report")?;
        let opts = core_api::memory::brief::BriefOptions {
            budget: std::time::Duration::from_millis(budget_ms),
        };
        let report = py.allow_threads(|| {
            self.db
                .with_ref(|db| Ok(core_api::memory::schema::schema_report(db, &opts)))
        })?;
        report_to_py(py, serde_json::to_value(&report))
    }

    /// Forget a node, one property, or one fact — exactly one of the three.
    ///
    /// - `forget(key=k)` tombstones the node and every edge on it.
    /// - `forget(key=k, prop=p)` removes one property.
    /// - `forget(fact={"subject", "predicate", "object"})` retracts one
    ///   hand-written edge.
    ///
    /// Returns the report as a dict: `mode` (`"node"`, `"prop"` or
    /// `"fact"`), `target`, `changed` (`False` when there was nothing to
    /// forget), `manual_edges`, `derived_edges`, `notes` and `notes_total`
    /// (notes that still say it — **listed, never deleted**), `history_floor`,
    /// `prop` in prop mode, `aliases_rewritten` (a forgotten `name` took its
    /// words out of `aliases` in the same commit) and `alias_keys_remain`
    /// (forgotten `aliases` left declared aliases behind in `alias_keys`).
    ///
    /// **This is a tombstone, not a redaction.** `node_history`,
    /// `edge_history`, `edges_at` and `was_linked` still read what was
    /// forgotten, from `history_floor` on, until the log is pruned.
    ///
    /// Raises `ValueError` for any other combination of arguments,
    /// `KeyNotFound` for an unknown key or fact endpoint, and `RuleOwned` for
    /// a fact a rule derived. That refusal's message and its `.detail` are
    /// the same whole sentence — which rule owns the edge and the fields it
    /// reads — with no `edge is rule-owned: ` prefix in front of it. Nothing
    /// is written in any of the three.
    #[pyo3(
        signature = (key = None, prop = None, fact = None),
        text_signature = "($self, key=None, prop=None, fact=None)"
    )]
    pub(crate) fn forget(
        &self,
        py: Python<'_>,
        key: Option<String>,
        prop: Option<String>,
        fact: Option<Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        use core_api::memory::forget::{forget, ForgetTarget, FORGET_SHAPE};
        self.db.refuse_if_scoped()?;
        let fact = match fact {
            Some(d) => Some((
                required_str(&d, "subject", "fact")?,
                required_str(&d, "predicate", "fact")?,
                required_str(&d, "object", "fact")?,
            )),
            None => None,
        };
        let Some(target) = ForgetTarget::from_parts(key, prop, fact) else {
            return Err(PyValueError::new_err(FORGET_SHAPE));
        };
        let is_fact = matches!(target, ForgetTarget::Fact { .. });
        // The engine's result comes out of `with_mut` unmapped, so the store
        // is released before any error is turned into an exception.
        let report = match self.db.with_mut(|db| Ok(forget(db, &target)))? {
            Ok(report) => report,
            // Only a fact's refusal is the enriched sentence, and it is the
            // whole message: the variant's `edge is rule-owned: ` prefix was
            // written for a one-word detail. The MCP tool answers the same.
            Err(GraphError::RuleOwned { detail }) if is_fact => {
                return Err(err_with(py, RuleOwned::new_err(detail.clone()), |v| {
                    v.setattr("detail", detail.as_str())
                }))
            }
            Err(e) => return Err(graph_err(e)),
        };
        report_to_py(py, serde_json::to_value(&report))
    }

    /// Which keys are one entity: `SAME_AS` links resolved into identities.
    ///
    /// Returns `{"clusters": [...], "linked": int, "claims": int, "floor":
    /// float}`. Each cluster is `{"canonical", "members", "weakest"}`: every
    /// pair of `members` is linked at `floor` or above, `canonical` is the
    /// oldest member and `members[0]`, and `weakest` is the lowest pairwise
    /// score inside it. `claims` counts unordered pairs — both directions of
    /// a link are one claim. `weakest` is the raw float
    /// (`0.6666666666666666`); the MCP tool prints it to two decimals.
    ///
    /// Empty on a store with no `SAME_AS` edges. The identity preset that
    /// derives them is applied with `mushroomdb schema apply <db>
    /// --memory-identity`, on the command line, with this handle closed; a
    /// `SAME_AS` rule made with `create_rule` is the other way.
    ///
    /// Raises `ValueError` for a `floor` that is not a finite number.
    ///
    /// Refused on a `scoped()` handle with `ValueError`.
    #[allow(deprecated)]
    #[pyo3(
        signature = (floor = core_api::memory::identity::SAME_AS_FLOOR),
        text_signature = "($self, floor=0.6)"
    )]
    pub(crate) fn identity_clusters(&self, py: Python<'_>, floor: f64) -> PyResult<Py<PyAny>> {
        self.db.refuse_read_if_scoped("identity_clusters")?;
        if !floor.is_finite() {
            return Err(PyValueError::new_err(format!(
                "identity_clusters: floor must be a finite number, got {floor}"
            )));
        }
        let report = py.allow_threads(|| {
            self.db
                .with_ref(|db| Ok(core_api::memory::identity::identity_clusters(db, floor)))
        })?;
        report_to_py(py, serde_json::to_value(&report))
    }
}
