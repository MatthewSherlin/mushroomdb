use core_api::{
    default_max_edges, valid_namespace, AlgoDir, AsOfScope, Direction, EdgeAt, Explanation,
    GraphDb as CoreDb, GraphError, HistoryChange, HistoryEntry, MaskedNodeResult, NamespaceStats,
    NodeInfo, NodeMask, OnConflict, PredicateSummary, PropPredicate, ResultSet, RuleDef, Scope,
    Value, NS_MAX_LEN, NS_PROP,
};
use core_api::restore::{restore_if_empty, RestoreOutcome};
use core_storage::fs::RealFs;
use pyo3::exceptions::{PyBaseException, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type Db = CoreDb<RealFs>;

struct Inner(Mutex<Option<Db>>);

#[pyclass(name = "GraphDb")]
struct GraphDb {
    /// The store, shared with every handle `scoped()` produced from this one.
    /// One store, one mutex: a child is an allocation, not a second open.
    inner: Arc<Inner>,
    /// What this handle may read, or `None` for an unscoped one.
    ///
    /// Held as a descriptor rather than a resolved [`NodeMask`] on purpose: a
    /// mask cached here would be the allow-list the store had when the handle
    /// was built, and a handle held across a write would serve it afterwards.
    /// That is a leak, not a staleness bug — so every read resolves again.
    scope: Option<Scope>,
}

#[pymethods]
impl GraphDb {
    /// Open (creating if needed) the database rooted at `path`.
    ///
    /// A read-write handle holds the store's cross-process write lock for as
    /// long as it is open, so only one such handle exists at a time across all
    /// processes. If another one is already open, this raises `MushroomBusy`.
    ///
    /// Pass `read_only=True` for a handle that never writes and never takes the
    /// lock: it opens immediately even while another process is writing, every
    /// mutation raises `RuntimeError`, and `refresh()` still follows the
    /// writer's commits. See the Concurrency section of the binding README.
    #[staticmethod]
    #[pyo3(signature = (path, read_only = false))]
    #[pyo3(text_signature = "(path, read_only=False)")]
    fn open(path: PathBuf, read_only: bool) -> PyResult<Self> {
        let opts = core_api::OpenOptions {
            read_only,
            ..core_api::OpenOptions::default()
        };
        let db = CoreDb::open_with_options(&path, opts).map_err(graph_err)?;
        Ok(GraphDb {
            inner: Arc::new(Inner(Mutex::new(Some(db)))),
            scope: None,
        })
    }

    /// A child handle that applies a read scope to **every** read and refuses
    /// every write.
    ///
    /// It shares this handle's store and mutex: it is not a second open, it
    /// takes no lock, and it costs one small allocation. `close()` on either
    /// name closes the one store they share.
    ///
    /// At least one of `role`, `namespace` and `keys` is required — an empty
    /// scope is a `ValueError`, never a handle that quietly reads everything.
    /// Present legs **intersect**, so `scoped()` on a scoped handle narrows
    /// further and can never widen. `keys=[]` is a leg: it narrows to nothing.
    ///
    /// The contract every read obeys:
    ///
    /// > The subject is checked first, so a key outside the scope is
    /// > indistinguishable from a key that does not exist. Then every other
    /// > node the answer would mention — neighbour, endpoint, candidate,
    /// > evidence — is filtered to the scope.
    ///
    /// The scope is resolved **per read**, so a handle held across a write
    /// answers from the store as it is now: a key created since is visible, a
    /// key deleted since is not. An unknown `role` raises here rather than on
    /// the first read, so a typo cannot produce a working-looking handle.
    ///
    /// `refresh()` is permitted — it writes nothing. `has_vector_rule` and
    /// `is_index_enabled` answer unscoped: they are schema facts, not node data.
    ///
    /// A read that answers about the whole store takes no mask, so it is
    /// **refused** on a scoped handle with `ValueError` rather than narrowed:
    /// `pagerank`, `connected_components`, `degree_centrality`, `communities`,
    /// `search`, `fulltext_pairs`, `rules`, `suggest_rules`, `recall`,
    /// `schema_report`, `identity_clusters` and `roles`. `is_fulltext_enabled`
    /// answers: it is a schema fact about a pair you named.
    ///
    /// ```python
    /// s = db.scoped(role="reader-a")
    /// t = db.scoped(namespace="tenant-a", keys=visible_ids)
    /// s.node_info("something-else-entirely")   # None, as for an absent key
    /// ```
    #[pyo3(
        signature = (role = None, namespace = None, keys = None),
        text_signature = "($self, role=None, namespace=None, keys=None)"
    )]
    fn scoped(
        &self,
        role: Option<String>,
        namespace: Option<String>,
        keys: Option<Vec<String>>,
    ) -> PyResult<GraphDb> {
        if role.is_none() && namespace.is_none() && keys.is_none() {
            return Err(PyValueError::new_err(
                "scoped() needs at least one of role, namespace or keys; an empty scope is \
                 refused rather than read as unscoped",
            ));
        }
        check_namespace(namespace.as_deref())?;
        let leg = Scope::new(role, namespace, keys).map_err(graph_err)?;
        let scope = match &self.scope {
            Some(parent) => parent.intersect(&leg),
            None => leg,
        };

        // Resolve once, eagerly, and throw the mask away. `Scope::new` takes no
        // store, so it cannot tell a real role name from a typo; without this
        // the refusal would surface on some later read instead, on a handle the
        // caller has already handed out. Per-read resolution is unchanged — the
        // mask this builds is deliberately not kept.
        self.with_ref(|db| scope.resolve(db).map(|_| ()))?;

        Ok(GraphDb {
            inner: Arc::clone(&self.inner),
            scope: Some(scope),
        })
    }

    /// Apply everything other processes have committed since this handle last
    /// looked, and return how many commits were applied.
    ///
    /// A handle does not poll the store, so another process's writes stay
    /// invisible until you call this. Rules fire and derived edges appear
    /// exactly as they would on a fresh open. Nothing is written, so a
    /// `read_only=True` handle can refresh freely.
    ///
    /// A commit another process is still writing is left for the next call.
    ///
    /// A scoped handle may refresh: it writes nothing, and the scope applies to
    /// what the refreshed store then answers.
    #[pyo3(text_signature = "($self)")]
    fn refresh(&self) -> PyResult<u64> {
        self.with_mut_unscoped(|db| db.refresh())
    }

    /// Insert a new node.  Raises `RuntimeError` (`DuplicateKey`) if `key` is
    /// already live; use `upsert_node` for insert-or-update semantics.
    ///
    /// `namespace` is the namespace the node is created in — the reserved `ns`
    /// property, named on the call. Omitted (or `"default"`) means the `default`
    /// namespace and stores nothing, so a store that never passes one has no
    /// `ns` column at all. A namespace is set at insert and cannot be changed:
    /// `set_prop(key, "ns", …)` on an existing node raises.
    ///
    /// ```python
    /// db.insert_node("Doc", "a1", {"title": "t"}, namespace="tenant-a")
    /// ```
    #[pyo3(
        signature = (label, key, props, namespace = None),
        text_signature = "($self, label, key, props, namespace=None)"
    )]
    fn insert_node(
        &self,
        label: &str,
        key: &str,
        props: Bound<'_, PyDict>,
        namespace: Option<&str>,
    ) -> PyResult<()> {
        let mut mapped = dict_to_props(&props)?;
        if let Some(ns) = check_namespace(namespace)? {
            // A `props["ns"]` that disagrees with the argument is a caller that
            // has not decided which namespace it meant.
            match mapped.iter().find(|(f, _)| f == NS_PROP) {
                Some((_, Value::Str(s))) if s == ns => {}
                Some((_, existing)) => {
                    return Err(PyValueError::new_err(format!(
                        "insert_node: props ns is {existing:?} but namespace={ns:?}; a node is \
                         created in one namespace, so pass one or the other"
                    )))
                }
                None => mapped.push((NS_PROP.to_string(), Value::Str(ns.to_string()))),
            }
        }
        self.with_mut(|db| db.insert_node(label, key, mapped))
    }

    /// Insert `key` if absent, otherwise update it in place.
    ///
    /// Returns `"inserted"` or `"updated"`.  On update, only the fields
    /// present in `props` whose value differs from the stored one are written
    /// — fields you do not pass are left untouched, and unchanged fields
    /// produce no WAL record (so rules do not re-fire needlessly). Changed
    /// fields are one `set_props` call, so a mid-list refusal leaves the node
    /// untouched.
    ///
    /// Raises `ValueError` if `key` already exists under a different label:
    /// relabelling a node is not an upsert, and silently ignoring the
    /// mismatch would hide a caller bug.
    ///
    /// ```python
    /// db.upsert_node("Person", "alice", {"team": "red"})   # "inserted"
    /// db.upsert_node("Person", "alice", {"team": "blue"})  # "updated"
    /// ```
    #[pyo3(text_signature = "($self, label, key, props)")]
    fn upsert_node(&self, label: &str, key: &str, props: Bound<'_, PyDict>) -> PyResult<String> {
        // The existence read below is unscoped, and its refusal names the
        // stored label — so on a scoped handle it answered "does this key
        // exist, and what is it" before `with_mut` ever refused the write.
        // The guard has to come first, exactly as `create_rule`'s does.
        self.refuse_if_scoped()?;
        let mapped = dict_to_props(&props)?;
        let existing = self.with_ref(|db| Ok(db.node_info(key)))?;
        let Some(info) = existing else {
            self.with_mut(|db| db.insert_node(label, key, mapped))?;
            return Ok("inserted".to_string());
        };
        if info.label != label {
            return Err(PyValueError::new_err(format!(
                "upsert_node: node '{key}' already exists with label '{}', not '{label}'; \
                 delete and re-insert to change a node's label",
                info.label
            )));
        }
        let mut to_set: Vec<(String, Value)> = Vec::new();
        for (field, value) in mapped {
            if info.props.get(&field) == Some(&value) {
                continue; // unchanged: no WAL record, no rule re-fire
            }
            to_set.push((field, value));
        }
        self.with_mut(|db| db.set_props(key, to_set))?;
        Ok("updated".to_string())
    }

    /// Insert a user-owned edge.  Returns `True` if it was newly written,
    /// `False` if it already existed.
    #[pyo3(text_signature = "($self, edge_type, src, dst)")]
    fn insert_edge(&self, edge_type: &str, src: &str, dst: &str) -> PyResult<bool> {
        self.with_mut(|db| db.insert_edge(edge_type, src, dst))
    }

    /// Delete a user-owned edge.  Returns `True` if the edge existed and was
    /// removed, `False` if it was not present.  Raises `RuntimeError` if the
    /// edge is rule-derived (must retract by changing properties instead).
    #[pyo3(text_signature = "($self, edge_type, src, dst)")]
    fn delete_edge(&self, edge_type: &str, src: &str, dst: &str) -> PyResult<bool> {
        self.with_mut(|db| db.delete_edge(edge_type, src, dst))
    }

    /// Delete a live node and every edge incident on it.
    ///
    /// Returns a `DeleteReport` dict `{"manual_edges": N, "derived_edges": M}`
    /// counting the user-inserted and rule-derived edges removed.  Raises
    /// `RuntimeError` (`KeyNotFound`) for an unknown or already-deleted key.
    ///
    /// ```python
    /// report = db.delete_node("alice")
    /// # {"manual_edges": 1, "derived_edges": 3}
    /// ```
    #[pyo3(text_signature = "($self, key)")]
    fn delete_node(&self, py: Python<'_>, key: &str) -> PyResult<Py<PyDict>> {
        let report = self.with_mut(|db| db.delete_node(key))?;
        let d = PyDict::new(py);
        d.set_item("manual_edges", report.manual_edges)?;
        d.set_item("derived_edges", report.derived_edges)?;
        Ok(d.unbind())
    }

    /// Set or overwrite a single property.
    ///
    /// `value=None` removes the field (equivalent to `remove_prop`) — Python
    /// has no distinct "null property" and the store has no null `Value`, so
    /// `None` means absent.
    #[pyo3(text_signature = "($self, key, field, value)")]
    fn set_prop(&self, key: &str, field: &str, value: Bound<'_, PyAny>) -> PyResult<()> {
        if value.is_none() {
            self.with_mut(|db| db.remove_prop(key, field))?;
            return Ok(());
        }
        let v = py_to_value(&value)?;
        self.with_mut(|db| db.set_prop(key, field, v))
    }

    /// Remove a property.  Returns `True` if the field was present and
    /// removed, `False` if it was already absent.  Raises `RuntimeError`
    /// (`KeyNotFound`) for an unknown or deleted key.
    ///
    /// Removing a field a rule watches retracts the edges that field derived.
    #[pyo3(text_signature = "($self, key, field)")]
    fn remove_prop(&self, key: &str, field: &str) -> PyResult<bool> {
        self.with_mut(|db| db.remove_prop(key, field))
    }

    /// Execute a read query, optionally with named parameters.
    ///
    /// `params` may be:
    /// - omitted or `None` (no parameters)
    /// - a `dict` mapping name→value (ergonomic form)
    /// - a list of `(name, value)` tuples (back-compat with `query_with_params`)
    ///
    /// Values must be `int`, `float`, `str`, `bool`, `list`, or `dict`.
    /// Parameters are bound, never interpolated, so string values are safe
    /// against injection.
    ///
    /// Returns one dict per row, keyed by RETURN alias.
    ///
    /// `role` answers as one of the store's roles (from `roles.json`) and
    /// `namespace` from one namespace only. They **intersect** — a namespace can
    /// only narrow what a role already allows, so a role bound to `tenant-a`
    /// asked for `tenant-b` answers with nothing — and either one makes the call
    /// a read, so a write statement raises.
    ///
    /// ```python
    /// rows = db.query(
    ///     "MATCH (n:Person) WHERE n.age > $min RETURN key(n) AS id",
    ///     {"min": 18},
    /// )
    /// mine = db.query("MATCH (n) RETURN n", role="a-reader", namespace="tenant-a")
    /// ```
    #[pyo3(
        signature = (cypher, params = None, role = None, namespace = None),
        text_signature = "($self, cypher, params=None, role=None, namespace=None)"
    )]
    fn query(
        &self,
        py: Python<'_>,
        cypher: &str,
        params: Option<Bound<'_, PyAny>>,
        role: Option<&str>,
        namespace: Option<&str>,
    ) -> PyResult<Vec<Py<PyDict>>> {
        let map = params_to_map(params)?;
        let namespace = check_namespace(namespace)?;
        let rs = self.with_scope(|db, scope_mask| {
            // One mask per leg, intersected — the same never-widen composition
            // every other surface uses. A role bound to namespaces honours them
            // with no `namespace` here; one outside its binding is the empty
            // intersection, never the union. Either argument makes the call a
            // read: a masked write raises.
            let call_mask = match (role, namespace) {
                (Some(role), Some(ns)) => Some(
                    db.mask_for_role(role)?
                        .intersect(&db.mask_for_namespace(ns)),
                ),
                (Some(role), None) => Some(db.mask_for_role(role)?),
                (None, Some(ns)) => Some(db.mask_for_namespace(ns)),
                (None, None) => None,
            };
            match narrow(scope_mask, call_mask) {
                Some(mask) => db.query_masked(cypher, &map, &mask),
                None => db.query(cypher, &map),
            }
        })?;
        result_set_to_rows(py, &rs)
    }

    /// Execute a read query with named parameters (back-compat alias for
    /// `query(cypher, params=[...])` with a tuple-list).
    ///
    /// ```python
    /// rows = db.query_with_params(
    ///     "MATCH (n:Person) WHERE n.age > $min RETURN key(n)",
    ///     [("min", 18)],
    /// )
    /// ```
    ///
    /// Each element of `params` is a `(name, value)` tuple.  Values must be
    /// `int`, `float`, `str`, `bool`, or a `list` of those.
    #[pyo3(text_signature = "($self, cypher, params)")]
    fn query_with_params(
        &self,
        py: Python<'_>,
        cypher: &str,
        params: Bound<'_, PyList>,
    ) -> PyResult<Vec<Py<PyDict>>> {
        let mut map = BTreeMap::new();
        for item in params.iter() {
            let tuple = item.downcast::<pyo3::types::PyTuple>().map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err(
                    "params must be a list of (name, value) tuples",
                )
            })?;
            if tuple.len() != 2 {
                return Err(pyo3::exceptions::PyTypeError::new_err(
                    "each param must be a (name, value) tuple",
                ));
            }
            let name: String = tuple.get_item(0)?.extract()?;
            let val = py_to_value(&tuple.get_item(1)?)?;
            map.insert(name, val);
        }
        let rs = self.with_scope(|db, mask| match mask {
            Some(mask) => db.query_masked(cypher, &map, mask),
            None => db.query(cypher, &map),
        })?;
        result_set_to_rows(py, &rs)
    }

    /// Rename a node's key.  The dense id (edges, history, last-change) is
    /// unchanged.
    ///
    /// Raises `RuntimeError` with `KeyNotFound` if `old` is unknown, or
    /// `DuplicateKey` if `new` is already live.
    #[pyo3(text_signature = "($self, old, new)")]
    fn rename_node(&self, old: &str, new: &str) -> PyResult<()> {
        self.with_mut(|db| db.rename_node(old, new))
    }

    /// Insert an edge, auto-creating any missing endpoint.
    ///
    /// Each missing endpoint is created as a plain node with label
    /// `placeholder_label` and no properties.  Rules fire and last-change is
    /// updated for each auto-created node.  Returns a dict with keys
    /// `nodes_created` and `edge_inserted`.
    #[pyo3(text_signature = "($self, edge_type, src, dst, placeholder_label)")]
    fn insert_edge_upsert(
        &self,
        py: Python<'_>,
        edge_type: &str,
        src: &str,
        dst: &str,
        placeholder_label: &str,
    ) -> PyResult<Py<PyDict>> {
        let (nodes, edges) = self.with_mut(|db| {
            db.batch()
                .insert_edge_upsert(edge_type, src, dst, placeholder_label)
                .commit()
        })?;
        let d = PyDict::new(py);
        d.set_item("nodes_created", nodes)?;
        d.set_item("edge_inserted", edges > 0)?;
        Ok(d.unbind())
    }

    /// Execute a Cypher write statement (CREATE / MATCH…SET / MATCH…DELETE /
    /// MATCH…DETACH DELETE / MERGE).
    ///
    /// Returns a one-row result dict with keys `created`, `properties_set`,
    /// and `deleted`, unless the statement has its own `RETURN` projection.
    ///
    /// `params` takes the same shapes as `query`: `None`, a `dict`, or a list
    /// of `(name, value)` tuples.
    ///
    /// ```python
    /// db.query_write(
    ///     "MATCH (n:Person) WHERE key(n) = $k SET n.age = 31 RETURN key(n)",
    ///     {"k": "alice"},
    /// )
    /// ```
    #[pyo3(signature = (cypher, params = None), text_signature = "($self, cypher, params=None)")]
    fn query_write(
        &self,
        py: Python<'_>,
        cypher: &str,
        params: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Vec<Py<PyDict>>> {
        let map = params_to_map(params)?;
        let rs = self.with_mut(|db| db.query_write(cypher, &map))?;
        result_set_to_rows(py, &rs)
    }

    /// Returns `True` if any approximate (HNSW) VectorSimilar rule covers
    /// `field`.
    ///
    /// Use as a capability probe before calling `find_similar`: when `True`,
    /// the native ANN index is active and `find_similar` will use it;
    /// when `False`, no HNSW rule covers `field` and `find_similar` falls back
    /// to an O(n) brute-force scan.
    ///
    /// ```python
    /// if db.has_vector_rule("embedding"):
    ///     hits = db.find_similar("embedding", query_vec, k=10)
    /// ```
    #[pyo3(text_signature = "($self, field)")]
    fn has_vector_rule(&self, field: &str) -> PyResult<bool> {
        self.with_ref(|db| Ok(db.has_vector_rule(field)))
    }

    /// Find the `k` most similar nodes to `vector` by cosine similarity on
    /// `field`.
    ///
    /// When `label` is `None` (the default) the search spans nodes of every
    /// label; when `label` is a string it restricts to nodes with that label.
    ///
    /// Uses the HNSW index when a VectorSimilar rule covers `field`
    /// (check with `has_vector_rule`); falls back to O(n) brute-force scan
    /// when no such rule exists.
    ///
    /// When `mask` is a list of node keys, only those nodes are eligible for
    /// results (hidden nodes are excluded before k-truncation).  `None` keeps
    /// the existing unmasked behaviour.
    ///
    /// `where` is an optional property predicate (`{"field": ..., "eq": ...}`
    /// or `{"field": ..., "in": [...]}`) and implies exact search. `exact=True`
    /// skips HNSW and GEMM-brutes the candidate set.
    ///
    /// Returns a list of `(node_key, similarity_score)` tuples sorted by
    /// score descending, filtered to `score >= min`.
    ///
    /// **`min` defaults to `0.8`**, the same floor the MCP `find_similar` tool
    /// and HTTP `POST /find_similar` apply. It was `0.0` here through 0.6: a
    /// call that never named `min` now drops every hit below `0.8`, and
    /// nothing raises. Pass `min=0.0` for the old behaviour.
    ///
    /// ```python
    /// hits = db.find_similar("embedding", query_vec, k=10, min=0.7)
    /// # restrict to a label:
    /// hits = db.find_similar("embedding", query_vec, label="Document", k=10)
    /// # restrict to visible nodes:
    /// hits = db.find_similar("embedding", query_vec, mask=["alice", "bob"])
    /// hits = db.find_similar("embedding", query_vec, where={"field": "scope", "eq": "a"})
    /// ```
    #[allow(clippy::too_many_arguments)]
    #[allow(deprecated)]
    #[pyo3(
        signature = (field, vector, label = None, k = 10, min = core_api::FIND_SIMILAR_DEFAULT_MIN, mask = None, r#where = None, exact = false),
        text_signature = "($self, field, vector, label=None, k=10, min=0.8, mask=None, where=None, exact=False)"
    )]
    fn find_similar(
        &self,
        py: Python<'_>,
        field: &str,
        vector: Bound<'_, PyList>,
        label: Option<&str>,
        k: usize,
        min: f64,
        mask: Option<Bound<'_, PyList>>,
        r#where: Option<Bound<'_, PyDict>>,
        exact: bool,
    ) -> PyResult<Vec<(String, f64)>> {
        let q = pylist_to_f64_vec(&vector)?;
        let field = field.to_owned();
        let label = label.map(str::to_owned);
        let pred = match r#where {
            Some(d) => Some(py_to_where(&d)?),
            None => None,
        };
        let exact = exact || pred.is_some();
        let mask_keys = if let Some(mask_list) = mask {
            let mut keys: Vec<String> = Vec::with_capacity(mask_list.len());
            for item in mask_list.iter() {
                if let Ok(s) = item.downcast::<PyString>() {
                    keys.push(s.to_string());
                } else {
                    return Err(PyTypeError::new_err("mask must be a list of strings"));
                }
            }
            Some(keys)
        } else {
            None
        };
        py.allow_threads(|| {
            self.with_scope(|db, scope_mask| {
                let call_mask = mask_keys
                    .as_ref()
                    .map(|keys| NodeMask::from_keys(db, keys.iter().map(String::as_str)));
                let node_mask = narrow(scope_mask, call_mask);
                db.find_similar_vector_filtered(
                    &field,
                    label.as_deref(),
                    &q,
                    k,
                    min,
                    node_mask.as_ref(),
                    pred.as_ref(),
                    exact,
                )
            })
        })
    }

    /// Exact per-key cosine top-k among `keys`. Self excluded. No HNSW.
    ///
    /// Unknown keys, missing embeddings, zero-norm and wrong-dim vectors are
    /// skipped. Duplicate keys collapse to first-seen order. Empty `keys`
    /// returns `[]`.
    ///
    /// ```python
    /// hits = db.pairwise_similar(["a", "b", "c"], "embedding", k=5, min=0.0)
    /// ```
    #[allow(deprecated)]
    #[pyo3(
        signature = (keys, field, k = 10, min = 0.0),
        text_signature = "($self, keys, field, k=10, min=0.0)"
    )]
    fn pairwise_similar(
        &self,
        py: Python<'_>,
        keys: Vec<String>,
        field: &str,
        k: usize,
        min: f64,
    ) -> PyResult<Vec<(String, Vec<(String, f64)>)>> {
        let field = field.to_owned();
        py.allow_threads(|| {
            self.with_scope(|db, mask| {
                let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
                match mask {
                    // Hidden keys leave the input before the matmul, not the
                    // answer afterwards: a hidden vector packed into the Gram
                    // can take a visible neighbour's place in the top-k.
                    Some(mask) => db.pairwise_similar_scoped(&refs, &field, k, min, mask),
                    None => db.pairwise_similar(&refs, &field, k, min),
                }
            })
        })
    }

    /// Hybrid RRF search combining fulltext and vector similarity.
    ///
    /// Fuses up to `4*k` fulltext hits on `text_field` for `query_text` with
    /// up to `4*k` vector hits on `vector_field` for `vector` using Reciprocal
    /// Rank Fusion (constant 60).  When `vector` is empty the vector leg is
    /// skipped and results come from the text leg alone.
    ///
    /// Returns `[(node_key, fused_score)]` sorted score-descending, ties by key.
    ///
    /// ```python
    /// hits = db.search_hybrid(
    ///     "bio", "machine learning",
    ///     "embedding", query_vec,
    ///     label="Person", k=5,
    /// )
    /// ```
    #[allow(clippy::too_many_arguments)]
    #[pyo3(
        signature = (text_field, query_text, vector_field, vector, label = None, k = 10),
        text_signature = "($self, text_field, query_text, vector_field, vector, label=None, k=10)"
    )]
    fn search_hybrid(
        &self,
        _py: Python<'_>,
        text_field: &str,
        query_text: &str,
        vector_field: &str,
        vector: Bound<'_, PyList>,
        label: Option<&str>,
        k: usize,
    ) -> PyResult<Vec<(String, f64)>> {
        let q = pylist_to_f64_vec(&vector)?;
        self.with_scope(|db, mask| {
            Ok(match mask {
                // Both legs are filtered before the fusion, so the ranks that
                // enter RRF are the ranks of the visible corpus and `k` is
                // honoured. Filtering the fused list would quietly return fewer.
                Some(mask) => db.search_hybrid_scoped(
                    text_field,
                    query_text,
                    vector_field,
                    &q,
                    label,
                    k,
                    mask,
                ),
                None => db.search_hybrid(text_field, query_text, vector_field, &q, label, k),
            })
        })
    }

    /// Read a single property from an edge.
    ///
    /// Returns the value if the edge has this property set (e.g. a `score`
    /// weight written by a rule), or `None` if the edge does not exist, the
    /// field is absent, or any key cannot be resolved.
    ///
    /// ```python
    /// score = db.get_edge_prop("SIMILAR", "alice", "bob", "score")
    /// ```
    #[pyo3(text_signature = "($self, edge_type, src_key, dst_key, field)")]
    fn get_edge_prop(
        &self,
        py: Python<'_>,
        edge_type: &str,
        src_key: &str,
        dst_key: &str,
        field: &str,
    ) -> PyResult<Py<PyAny>> {
        let val = self.with_scope(|db, mask| {
            Ok(match mask {
                // An edge one of whose endpoints is hidden reads as an edge
                // that is not there, which is what an unresolvable key already
                // returns.
                Some(mask)
                    if !(mask.contains_node(db, src_key) && mask.contains_node(db, dst_key)) =>
                {
                    None
                }
                _ => db.get_edge_prop(edge_type, src_key, dst_key, field),
            })
        })?;
        match val {
            Some(v) => value_to_py(py, &v).map(|b| b.unbind()),
            None => Ok(py.None()),
        }
    }

    /// Register a linking rule.  Returns `True` when the rule was created.
    ///
    /// `rule` is a dict with `name`, `src_label`, `dst_label`, `predicate`,
    /// `edge_type`, and optionally `weight_prop`, `max_edges`, `approximate`,
    /// `via_label`, `via_edge`, `via_dir`.
    ///
    /// The `predicate` accepts two shapes.  The canonical one is the
    /// snake_case form that `explain` emits, so an explanation round-trips
    /// straight back into a new rule:
    ///
    /// ```python
    /// {"kind": "field_equal", "fields": ["team"]}
    /// {"kind": "overlap", "fields": ["skills"], "min": 0.5}
    /// {"kind": "all", "parts": [ …nested predicates… ]}
    /// ```
    ///
    /// The Rust-native externally-tagged form is also accepted:
    /// `{"FieldEqual": {"field": "team"}}`, `{"Overlap": {"field": "skills",
    /// "min": 0.5}}`, `{"All": [ … ]}`.
    ///
    /// With `if_not_exists=True`, a rule whose `name` is already registered
    /// returns `False` instead of raising.
    ///
    /// A `"namespace"` key scopes the rule to one namespace: it sees only that
    /// namespace's nodes — source, via hop and destination — so every edge it
    /// derives stays inside. Omitted means a global rule, the only kind that may
    /// derive an edge across a boundary.
    #[pyo3(
        signature = (rule, if_not_exists = false),
        text_signature = "($self, rule, if_not_exists=False)"
    )]
    fn create_rule(
        &self,
        py: Python<'_>,
        rule: Bound<'_, PyAny>,
        if_not_exists: bool,
    ) -> PyResult<bool> {
        // `if_not_exists` can return before the write path, so this one needs
        // its own refusal rather than `with_mut`'s.
        self.refuse_if_scoped()?;
        let def = rule_from_py(py, &rule)?;
        if if_not_exists {
            let name = def.name.clone();
            let exists = self.with_ref(|db| Ok(db.rules().iter().any(|r| r.name == name)))?;
            if exists {
                return Ok(false);
            }
        }
        self.with_mut(|db| db.create_rule(def))?;
        Ok(true)
    }

    /// Why are `a` and `b` linked?  Returns one dict per derived edge between
    /// them: `rule`, `edge_type`, `src_key`, `dst_key`, `weight`, `predicate`.
    ///
    /// `predicate` is the snake_case summary shape, which `create_rule`
    /// accepts verbatim.
    #[pyo3(text_signature = "($self, a, b)")]
    fn explain(&self, py: Python<'_>, a: &str, b: &str) -> PyResult<Vec<Py<PyDict>>> {
        let rows = self.with_scope(|db, mask| match mask {
            // An explanation whose evidence runs through a hidden node is
            // dropped whole, not redacted: it names the hop's edge type and
            // never the hop's key, so there is no field to blank.
            Some(mask) => db.explain_scoped(a, b, mask),
            None => db.explain(a, b),
        })?;
        rows.iter().map(|e| explanation_to_py(py, e)).collect()
    }

    /// One-hop neighbour keys along `edge_type`.  `direction` is `"out"` or
    /// `"in"`.
    #[pyo3(text_signature = "($self, key, edge_type, direction)")]
    fn neighbors(&self, key: &str, edge_type: &str, direction: &str) -> PyResult<Vec<String>> {
        let dir = parse_dir(direction)?;
        self.with_scope(|db, mask| match mask {
            // The one-hop shape of `neighborhood`: subject first, then the
            // neighbours themselves. At depth 1 "never crosses a hidden node"
            // and "never names one" are the same filter.
            Some(mask) => {
                if !mask.contains_node(db, key) {
                    return Err(GraphError::KeyNotFound { key: key.into() });
                }
                Ok(db
                    .neighbors(key, edge_type, dir)?
                    .into_iter()
                    .filter(|n| mask.contains_node(db, n))
                    .collect())
            }
            None => db.neighbors(key, edge_type, dir),
        })
    }

    /// Unique directed degree of `key`. `direction` is `"out"`, `"in"`, or
    /// `"both"` (out + in sum). Unknown `edge_type` is 0. Unknown key raises.
    ///
    /// `multiplicity=True` sums each pair's insert count instead of counting
    /// each pair once. It defaults to `False`, so an existing caller keeps
    /// unique-neighbour semantics, and it never raises: on a store that has not
    /// called `enable_multiplicity()` every pair counts 1, which returns the
    /// unique degree. The argument is a readout preference, not a demand the
    /// store has to be able to meet.
    #[allow(deprecated)]
    #[pyo3(
        signature = (key, edge_type = None, direction = "both", multiplicity = false),
        text_signature = "($self, key, edge_type=None, direction='both', multiplicity=False)"
    )]
    fn degree(
        &self,
        py: Python<'_>,
        key: &str,
        edge_type: Option<&str>,
        direction: &str,
        multiplicity: bool,
    ) -> PyResult<u64> {
        let dir = parse_algo_dir(direction)?;
        let key = key.to_owned();
        let edge_type = edge_type.map(str::to_owned);
        py.allow_threads(|| {
            self.with_scope(|db, mask| match (mask, multiplicity) {
                // Counting hidden neighbours would disclose their existence by
                // arithmetic — the same leak the edge filter prevents, and a
                // multiplicity count discloses more: not only that a hidden
                // neighbour exists but how often it was written.
                (Some(mask), true) => {
                    db.degree_scoped_multiplicity(&key, edge_type.as_deref(), dir, mask)
                }
                (Some(mask), false) => db.degree_scoped(&key, edge_type.as_deref(), dir, mask),
                (None, true) => db.degree_multiplicity(&key, edge_type.as_deref(), dir),
                (None, false) => db.degree(&key, edge_type.as_deref(), dir),
            })
        })
    }

    /// Unique directed degree for a key subset or a label scan.
    ///
    /// Unknown keys are omitted. `keys=[]` returns `[]`. `where` is the same
    /// dict shape as `find_similar`. `limit` applies after sorting degree
    /// descending, key ascending.
    ///
    /// `multiplicity=True` reports each row's insert-count sum rather than its
    /// unique neighbour count, with the same default and the same no-raise
    /// contract `degree` documents. The sort and `limit` then run over the
    /// counts that reading produces.
    #[allow(clippy::too_many_arguments)]
    #[allow(deprecated)]
    #[pyo3(
        signature = (keys = None, label = None, r#where = None, edge_type = None, direction = "both", limit = None, multiplicity = false),
        text_signature = "($self, keys=None, label=None, where=None, edge_type=None, direction='both', limit=None, multiplicity=False)"
    )]
    fn degrees(
        &self,
        py: Python<'_>,
        keys: Option<Vec<String>>,
        label: Option<&str>,
        r#where: Option<Bound<'_, PyDict>>,
        edge_type: Option<&str>,
        direction: &str,
        limit: Option<usize>,
        multiplicity: bool,
    ) -> PyResult<Vec<(String, u64)>> {
        let dir = parse_algo_dir(direction)?;
        let pred = match r#where {
            Some(d) => Some(py_to_where(&d)?),
            None => None,
        };
        let label = label.map(str::to_owned);
        let edge_type = edge_type.map(str::to_owned);
        py.allow_threads(|| {
            self.with_scope(|db, mask| match (mask, multiplicity) {
                // Hidden keys leave both sides: the input — whether they
                // arrived in `keys` or came out of the label/`where` scan — and
                // every row's count.
                (Some(mask), true) => db.degrees_scoped_multiplicity(
                    keys.as_deref(),
                    label.as_deref(),
                    pred.as_ref(),
                    edge_type.as_deref(),
                    dir,
                    limit,
                    mask,
                ),
                (Some(mask), false) => db.degrees_scoped(
                    keys.as_deref(),
                    label.as_deref(),
                    pred.as_ref(),
                    edge_type.as_deref(),
                    dir,
                    limit,
                    mask,
                ),
                (None, true) => db.degrees_multiplicity(
                    keys.as_deref(),
                    label.as_deref(),
                    pred.as_ref(),
                    edge_type.as_deref(),
                    dir,
                    limit,
                ),
                (None, false) => db.degrees(
                    keys.as_deref(),
                    label.as_deref(),
                    pred.as_ref(),
                    edge_type.as_deref(),
                    dir,
                    limit,
                ),
            })
        })
    }

    /// Unknown key: `None`, matching Rust `GraphDb::node_info` → `Option`.
    /// Contrast `node_edges`, which raises `RuntimeError` for the same miss
    /// because Rust returns `Result` (`GraphError::KeyNotFound`). Deliberate.
    ///
    /// Returns `{"key", "label", "props"}`.
    #[pyo3(text_signature = "($self, key)")]
    fn node_info(&self, py: Python<'_>, key: &str) -> PyResult<Option<Py<PyDict>>> {
        let info = self.with_scope(|db, mask| {
            Ok(match mask {
                // `Omit` mode, always: a hidden node reads as an absent one
                // rather than as a restricted stub, which would disclose that
                // it exists.
                Some(mask) => match db.node_info_masked(key, mask) {
                    Some(MaskedNodeResult::Visible(info)) => Some(info),
                    _ => None,
                },
                None => db.node_info(key),
            })
        })?;
        match info {
            Some(info) => Ok(Some(node_info_to_py(py, &info)?)),
            None => Ok(None),
        }
    }

    /// Unknown key: `RuntimeError` (`node key not found: …`), matching Rust
    /// `GraphDb::node_edges` → `Result`. `node_info` stays `None` on the same
    /// miss (`Option`). The asymmetry is the core API, not a Python invention.
    ///
    /// Each dict is `{"edge_type", "src_key", "dst_key", "derived"}`.
    #[pyo3(text_signature = "($self, key)")]
    fn node_edges(&self, py: Python<'_>, key: &str) -> PyResult<Vec<Py<PyDict>>> {
        let edges = self.with_scope(|db, mask| match mask {
            // Hidden subject → `KeyNotFound`, exactly as an unknown key; then
            // every edge naming a hidden endpoint is dropped.
            Some(mask) => db.node_edges_scoped(key, mask),
            None => db.node_edges(key),
        })?;
        edges
            .iter()
            .map(|e| {
                let d = PyDict::new(py);
                d.set_item("edge_type", &e.edge_type)?;
                d.set_item("src_key", &e.src_key)?;
                d.set_item("dst_key", &e.dst_key)?;
                d.set_item("derived", e.derived)?;
                Ok(d.unbind())
            })
            .collect()
    }

    /// Enable an equality index on `(label, field)` so `MATCH (n:label {field: v})`
    /// becomes an indexed lookup instead of a scan.
    #[pyo3(text_signature = "($self, label, field)")]
    fn enable_index(&self, label: &str, field: &str) -> PyResult<()> {
        self.with_mut(|db| db.enable_index(label, field))
    }

    /// Disable the equality index on `(label, field)`.
    #[pyo3(text_signature = "($self, label, field)")]
    fn disable_index(&self, label: &str, field: &str) -> PyResult<()> {
        self.with_mut(|db| db.disable_index(label, field))
    }

    /// Whether `(label, field)` currently has an equality index.
    #[pyo3(text_signature = "($self, label, field)")]
    fn is_index_enabled(&self, label: &str, field: &str) -> PyResult<bool> {
        self.with_ref(|db| Ok(db.is_index_enabled(label, field)))
    }

    /// Start recording insert-count multiplicity on this store.
    ///
    /// Adjacency stays a set: a duplicate `insert_edge` still returns `False`
    /// and `degree()` is unchanged. What it gains is that the duplicate is
    /// counted, readable as `degree(..., multiplicity=True)` and as the reserved
    /// `count` edge property.
    ///
    /// **This is a one-way step and there is no call that undoes it.** The count
    /// is durable, so it is written to the write-ahead log as a record no
    /// release before this one knows how to read; a store that has recorded one
    /// can no longer be read by an older binary. A store that never calls this
    /// writes no such record and stays readable. Calling it twice writes
    /// nothing the second time.
    ///
    /// The call also takes a snapshot, at a format version older releases do
    /// not know, before it writes that record — so an older binary refuses the
    /// store by name instead of silently truncating its write-ahead log. On a
    /// large store this costs one full snapshot write. History stays reachable:
    /// the snapshot keeps the log rather than truncating it.
    ///
    /// **It is not atomic.** If it raises, the store may be opted in anyway —
    /// the record can already be in the log with only its durability barrier
    /// having failed, or the snapshot can be enough on its own for the next
    /// open to finish the job. Every such state is one an older binary refuses
    /// by name rather than truncates, so nothing is lost; but the exception
    /// means "outcome unknown", not "nothing happened". Reopen the store and
    /// call `is_multiplicity_enabled()` to find out where it stands.
    #[pyo3(text_signature = "($self)")]
    fn enable_multiplicity(&self) -> PyResult<()> {
        self.with_mut(|db| db.enable_multiplicity())
    }

    /// Whether this store records insert-count multiplicity.
    #[pyo3(text_signature = "($self)")]
    fn is_multiplicity_enabled(&self) -> PyResult<bool> {
        self.with_ref(|db| Ok(db.is_multiplicity_enabled()))
    }

    /// Record subsequent commits as having happened at `unix_ms`, or `None` to
    /// go back to the system clock.
    ///
    /// For backfilled history: a mirror importing rows that carry their own
    /// timestamps. Without it every imported commit is stamped "now", so a store
    /// holding a year of history answers every date question with
    /// `TimeBeforeFloor`. Sticky until changed or cleared. Import in
    /// chronological order — going backwards raises `CommitTimeNotMonotonic`.
    #[pyo3(text_signature = "($self, unix_ms)")]
    fn record_commits_at(&self, unix_ms: Option<i64>) -> PyResult<()> {
        self.with_mut(|db| db.record_commits_at(unix_ms))
    }

    /// The 0-based frame index a date resolves to.
    ///
    /// The last commit at or before `date`, ready to hand to `edges_at` or
    /// `was_linked`.
    #[pyo3(text_signature = "($self, date)")]
    fn resolve_date(&self, date: &str) -> PyResult<u64> {
        self.with_scope(|db, _| db.resolve_date(date))
    }

    /// The recorded wall-clock time of a 0-based frame index, in unix ms.
    ///
    /// `None` when the store records no time for it — a store written before
    /// 0.6.11, a frame below the horizon, or a damaged sidecar.
    #[pyo3(text_signature = "($self, commit)")]
    fn commit_time_ms(&self, commit: u64) -> PyResult<Option<i64>> {
        self.with_scope(|db, _| Ok(db.commit_time_ms(commit)))
    }

    /// Whether `a` and `b` were linked by `edge_type` at or before `at_commit`.
    ///
    /// `at_commit` takes a 0-based frame index, or an RFC 3339 date string
    /// (`"2026-06-19"`, `"2026-06-19T12:00:00Z"`) which resolves to the last
    /// commit at or before that instant.
    #[pyo3(text_signature = "($self, a, b, edge_type, at_commit)")]
    fn was_linked(
        &self,
        a: &str,
        b: &str,
        edge_type: &str,
        at_commit: Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let at_commit = self.resolve_when(&at_commit, "at_commit")?;
        self.with_scope(|db, mask| {
            // The real call runs first so an out-of-range `at_commit` raises
            // for a hidden pair exactly as it does for an unknown one; only
            // then is the answer narrowed. An unknown key here is `False`, not
            // an error, so a hidden one is `False` too — raising would say
            // "this key exists but you may not see it".
            let linked = db.was_linked(a, b, edge_type, at_commit)?;
            Ok(match mask {
                Some(mask) => linked && mask.contains_node(db, a) && mask.contains_node(db, b),
                None => linked,
            })
        })
    }

    /// Time-travel read: run `cypher` against the graph as it existed at
    /// `commit` (a 0-based WAL commit index). Read-only; the live store is
    /// unaffected. Returns a list of row dicts, like `query`.
    ///
    /// `role` and `namespace` intersect the same way as live `query`: a
    /// namespace can only narrow what a role already allows. An unknown role
    /// raises the same error live `query` raises.
    #[pyo3(
        signature = (commit, cypher, params=None, role=None, namespace=None),
        text_signature = "($self, commit, cypher, params=None, role=None, namespace=None)"
    )]
    fn query_at(
        &self,
        py: Python<'_>,
        commit: u64,
        cypher: &str,
        params: Option<Bound<'_, PyAny>>,
        role: Option<&str>,
        namespace: Option<&str>,
    ) -> PyResult<Vec<Py<PyDict>>> {
        let map = params_to_map(params)?;
        let namespace = check_namespace(namespace)?;
        // On a scoped handle the whole scope goes down, narrowed by any
        // per-call legs: `AsOfScope` names one restriction and cannot spell a
        // nested scope's several. The legs resolve against the as-of graph, so
        // a resolved live mask would be the wrong answer, not just a stale one.
        let rs = match self.scope_narrowed_by(role, namespace)? {
            Some(scope) => {
                self.with_ref(|db| db.query_at_with_scope(commit, cypher, &map, &scope))?
            }
            None if role.is_some() || namespace.is_some() => {
                self.with_ref(|db| match (role, namespace) {
                    (Some(role), Some(ns)) => db.query_at_scoped_in_namespace(
                        commit,
                        cypher,
                        &map,
                        AsOfScope::Role(role),
                        ns,
                    ),
                    (Some(role), None) => {
                        db.query_at_scoped(commit, cypher, &map, AsOfScope::Role(role))
                    }
                    (None, Some(ns)) => {
                        db.query_at_scoped(commit, cypher, &map, AsOfScope::Namespace(ns))
                    }
                    (None, None) => unreachable!("one of the two is Some in this branch"),
                })?
            }
            None => self.with_ref(|db| db.query_at(commit, cypher, &map))?,
        };
        result_set_to_rows(py, &rs)
    }

    /// Per-node change history. Returns `{key, history, total_commits,
    /// horizon}`; `history` is a list of `{commit, kind, ...}` dicts (kind is
    /// one of node_inserted, prop_set, prop_removed, edge_added, edge_removed,
    /// node_deleted) and `horizon` is the oldest commit still retained —
    /// events before it were pruned and are not in `history`.
    #[pyo3(text_signature = "($self, key)")]
    fn node_history(&self, py: Python<'_>, key: &str) -> PyResult<Py<PyDict>> {
        let result = self.with_scope(|db, mask| {
            let mut result = db.node_history(key)?;
            if let Some(mask) = mask {
                if mask.contains_node(db, key) {
                    // Every other node the history names is an edge partner.
                    result.items.retain(|e| match &e.change {
                        HistoryChange::EdgeAdded { other, .. }
                        | HistoryChange::EdgeRemoved { other, .. } => mask.contains_node(db, other),
                        _ => true,
                    });
                } else {
                    // An unknown key has an empty history rather than an error,
                    // so a hidden one does too — `KeyNotFound` here would be an
                    // existence oracle in reverse.
                    result.items.clear();
                }
            }
            Ok(result)
        })?;
        let history = result
            .items
            .iter()
            .map(|e| history_entry_to_dict(py, e))
            .collect::<PyResult<Vec<Py<PyDict>>>>()?;
        let out = PyDict::new(py);
        out.set_item("key", key)?;
        out.set_item("history", history)?;
        out.set_item("total_commits", result.total_commits)?;
        out.set_item("horizon", result.horizon)?;
        Ok(out.unbind())
    }

    /// Total number of committed WAL frames visible in the current horizon
    /// window (the exclusive upper bound for `at_commit` in `was_linked` and
    /// `commit` in `query_at`).
    #[pyo3(text_signature = "($self)")]
    fn wal_total_commits(&self) -> PyResult<u64> {
        self.with_ref(|db| db.wal_total_commits())
    }

    /// Per-edge change history between `a` and `b`. Returns `{a, b, events:
    /// [{edge_type, commit, event, rule}], total_commits, horizon}`; `event` is
    /// `"Added"` or `"Retracted"`, `rule` is the rule name for derived edges
    /// and `None` for manually written ones, and `horizon` is the oldest commit
    /// still retained — events before it were pruned and are not in `events`.
    #[pyo3(text_signature = "($self, a, b)")]
    fn edge_history(&self, py: Python<'_>, a: &str, b: &str) -> PyResult<Py<PyDict>> {
        let result = self.with_scope(|db, mask| {
            let mut result = db.edge_history(a, b)?;
            if let Some(mask) = mask {
                // Every event here is about the pair, so one hidden endpoint
                // empties the list — the answer an unknown key already gives.
                if !(mask.contains_node(db, a) && mask.contains_node(db, b)) {
                    result.items.clear();
                }
            }
            Ok(result)
        })?;
        let events = result
            .items
            .iter()
            .map(|ev| {
                let d = PyDict::new(py);
                d.set_item("edge_type", &ev.edge_type)?;
                d.set_item("commit", ev.commit)?;
                let event_str = match ev.event {
                    core_api::EdgeEvent::Added => "Added",
                    core_api::EdgeEvent::Retracted => "Retracted",
                };
                d.set_item("event", event_str)?;
                d.set_item("rule", ev.rule.as_deref())?;
                Ok(d.unbind())
            })
            .collect::<PyResult<Vec<Py<PyDict>>>>()?;
        let out = PyDict::new(py);
        out.set_item("a", a)?;
        out.set_item("b", b)?;
        out.set_item("events", events)?;
        out.set_item("total_commits", result.total_commits)?;
        out.set_item("horizon", result.horizon)?;
        Ok(out.unbind())
    }

    /// Every edge incident on `key` at WAL commit `commit`, from one WAL scan.
    ///
    /// The bulk form of `was_linked`: one call answers "what did this node's
    /// relationships look like then", instead of one `edge_history` per
    /// candidate partner.
    ///
    /// Returns a list of `{edge_type, src, dst, derived, rule}` dicts sorted by
    /// `(edge_type, src, dst)`; `rule` is the deriving rule's name for a
    /// rule-owned edge and `None` for a manually written one. Endpoint keys are
    /// reported under the name each node carries today.
    ///
    /// `commit` outside `[0, wal_total_commits())` raises `RuntimeError`. An
    /// unknown key is not an error — it simply had no edges.
    #[pyo3(text_signature = "($self, key, commit)")]
    fn edges_at(
        &self,
        py: Python<'_>,
        key: &str,
        commit: Bound<'_, PyAny>,
    ) -> PyResult<Vec<Py<PyDict>>> {
        let commit = self.resolve_when(&commit, "commit")?;
        let edges = self.with_scope(|db, mask| {
            // Run first so an out-of-range `commit` raises for a hidden key as
            // it does for an unknown one.
            let edges = db.edges_at(key, commit)?;
            Ok(match mask {
                // An unknown key here is an empty list, not an error, so a
                // hidden subject is one too; then hidden endpoints drop out.
                // Endpoints are reported under the names they carry *today*, so
                // today's mask is the right one to test them against.
                Some(mask) if !mask.contains_node(db, key) => Vec::new(),
                Some(mask) => edges
                    .into_iter()
                    .filter(|e| {
                        mask.contains_node(db, &e.src_key) && mask.contains_node(db, &e.dst_key)
                    })
                    .collect(),
                None => edges,
            })
        })?;
        edges.iter().map(|e| edge_at_to_py(py, e)).collect()
    }

    /// The derived edges that would be retracted and derived if `key.field`
    /// were set to `value` — nothing is written and the store is unchanged.
    ///
    /// Returns `{"lost": [...], "gained": [...]}` where each entry has the same
    /// shape as `edges_at` (`{edge_type, src, dst, derived, rule}`). A change
    /// with no effect returns two empty lists.
    ///
    /// Raises `RuntimeError` for an unknown key or a view-owned field. `value`
    /// must not be `None`.
    #[pyo3(text_signature = "($self, key, field, value)")]
    fn what_if_set_prop(
        &self,
        py: Python<'_>,
        key: &str,
        field: &str,
        value: Bound<'_, PyAny>,
    ) -> PyResult<Py<PyDict>> {
        if value.is_none() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "what_if_set_prop value must not be None",
            ));
        }
        let v = py_to_value(&value)?;
        let wi = self.with_scope(|db, mask| match mask {
            Some(mask) => {
                // Unlike the history surfaces, `what_if_set_prop` *does* raise
                // `KeyNotFound` for an unknown key, so a hidden one raises too.
                if !mask.contains_node(db, key) {
                    return Err(GraphError::KeyNotFound { key: key.into() });
                }
                let mut wi = db.what_if_set_prop(key, field, v)?;
                let visible = |e: &EdgeAt| {
                    mask.contains_node(db, &e.src_key) && mask.contains_node(db, &e.dst_key)
                };
                wi.lost.retain(visible);
                wi.gained.retain(visible);
                Ok(wi)
            }
            None => db.what_if_set_prop(key, field, v),
        })?;
        let lost = wi
            .lost
            .iter()
            .map(|e| edge_at_to_py(py, e))
            .collect::<PyResult<Vec<Py<PyDict>>>>()?;
        let gained = wi
            .gained
            .iter()
            .map(|e| edge_at_to_py(py, e))
            .collect::<PyResult<Vec<Py<PyDict>>>>()?;
        let out = PyDict::new(py);
        out.set_item("lost", lost)?;
        out.set_item("gained", gained)?;
        Ok(out.unbind())
    }

    /// Atomically ingest `nodes` (each `{key, label, props}`) and optional
    /// `edges` (each `{edge_type, src, dst}`) in a single WAL commit.
    ///
    /// A bad edge (unknown endpoint, rule-owned, …) rejects the **entire**
    /// batch — nothing is committed and `RuntimeError` is raised.
    ///
    /// `on_conflict` says what a node key that is already taken means, so a
    /// mirror can be rebuilt onto a store that already has content:
    ///
    /// - `"error"` (default) — one duplicate key rejects the whole frame with
    ///   `DuplicateKey`. The behaviour before 0.6.10, unchanged.
    /// - `"skip"` — the stored node is left exactly as it is (properties,
    ///   label and edges) and counted in `skipped`.
    /// - `"replace"` — the key is kept and the node's properties become
    ///   **exactly** the supplied props: supplied fields are set, fields
    ///   absent from the supplied props are removed. Counted in `replaced`.
    ///   A supplied label that differs from the stored one is a row error, not
    ///   a silent relabel — relabelling is `rename_node`. So is a supplied
    ///   `ns` that would move the node between namespaces, and an omitted `ns`
    ///   on a node that is in one, since an absent `ns` names `default`.
    ///
    /// Two properties sit outside "exactly", because neither is the caller's
    /// to supply: `ns`, which is immutable, and any property a **view** owns,
    /// which is kept rather than removed. Supplying a view-owned field is a row
    /// error, so omitting it cannot be a request to delete it — and refusing
    /// the row instead would make `"replace"` impossible for every node a view
    /// has written to. Each field kept that way is counted in
    /// `kept_view_owned`; the row still counts in `replaced` and raises no row
    /// error, so that count is the only signal a rebuild gets that the stored
    /// node carries a field its frame did not describe.
    ///
    /// The frame stays atomic under every policy: skip and replace are decided
    /// during the batch's own validate pass, so it still applies wholly or not
    /// at all. Rows refused by `replace` change nothing and the rest commits.
    ///
    /// Edges take the argument too but it changes nothing for them: adjacency
    /// is a set, so a duplicate edge is already a silent no-op under every
    /// policy, and these edge dicts carry no properties to replace.
    ///
    /// Returns a dict matching `IngestReport` shape, plus the conflict counts:
    /// `{inserted, edges_inserted, skipped, replaced, kept_view_owned,
    /// row_errors, rules_created, skipped_fk_fields}`. `row_errors` is a list of
    /// `(index into nodes, why)`. `edges_inserted` counts only newly written
    /// edges; duplicate edges that already exist are silent no-ops and are NOT
    /// counted.
    ///
    /// **Performance note**: for large datasets keep each call to ≤10 000 nodes.
    /// A single call with 100 000+ nodes serialises one giant WAL frame whose
    /// fsync cost dominates and negates the batching benefit.  Chunk at the
    /// call site (e.g. `for chunk in batched(nodes, 10_000)`).
    #[allow(clippy::type_complexity)]
    #[pyo3(
        signature = (nodes, edges=None, on_conflict="error"),
        text_signature = "($self, nodes, edges=None, on_conflict='error')"
    )]
    fn ingest_batch(
        &self,
        py: Python<'_>,
        nodes: Bound<'_, PyList>,
        edges: Option<Bound<'_, PyList>>,
        on_conflict: &str,
    ) -> PyResult<Py<PyDict>> {
        let policy = parse_on_conflict(on_conflict)?;
        // Parse nodes.
        let mut node_ops: Vec<(String, String, Vec<(String, Value)>)> =
            Vec::with_capacity(nodes.len());
        for item in nodes.iter() {
            let d = item.downcast::<PyDict>().map_err(|_| {
                PyTypeError::new_err("each node must be a dict {key, label, props}")
            })?;
            let key: String = d
                .get_item("key")?
                .ok_or_else(|| PyValueError::new_err("node dict missing 'key'"))?
                .extract()?;
            let label: String = d
                .get_item("label")?
                .ok_or_else(|| PyValueError::new_err("node dict missing 'label'"))?
                .extract()?;
            let props_obj = d
                .get_item("props")?
                .ok_or_else(|| PyValueError::new_err("node dict missing 'props'"))?;
            let props_dict = props_obj
                .downcast::<PyDict>()
                .map_err(|_| PyTypeError::new_err("node 'props' must be a dict"))?;
            let props = dict_to_props(props_dict)?;
            node_ops.push((label, key, props));
        }

        // Parse edges.
        let mut edge_ops: Vec<(String, String, String)> = Vec::new();
        if let Some(edge_list) = edges {
            for item in edge_list.iter() {
                let d = item.downcast::<PyDict>().map_err(|_| {
                    PyTypeError::new_err("each edge must be a dict {edge_type, src, dst}")
                })?;
                let edge_type: String = d
                    .get_item("edge_type")?
                    .ok_or_else(|| PyValueError::new_err("edge dict missing 'edge_type'"))?
                    .extract()?;
                let src: String = d
                    .get_item("src")?
                    .ok_or_else(|| PyValueError::new_err("edge dict missing 'src'"))?
                    .extract()?;
                let dst: String = d
                    .get_item("dst")?
                    .ok_or_else(|| PyValueError::new_err("edge dict missing 'dst'"))?
                    .extract()?;
                edge_ops.push((edge_type, src, dst));
            }
        }

        // Commit atomically via BatchBuilder; capture the actual WAL counts.
        let outcome = self.with_mut(|db| {
            let mut batch = db.batch();
            for (label, key, props) in node_ops {
                batch.insert_node_on_conflict(&label, &key, props, policy);
            }
            for (edge_type, src, dst) in &edge_ops {
                batch.insert_edge(edge_type, src, dst);
            }
            batch.commit_outcome()
        })?;

        // Build IngestReport-shaped dict with accurate counts.
        let d = PyDict::new(py);
        d.set_item("inserted", outcome.nodes_inserted)?;
        d.set_item("edges_inserted", outcome.edges_inserted)?;
        d.set_item("skipped", outcome.skipped)?;
        d.set_item("replaced", outcome.replaced)?;
        d.set_item("kept_view_owned", outcome.kept_view_owned)?;
        d.set_item("row_errors", outcome.row_errors)?;
        d.set_item("rules_created", PyList::empty(py))?;
        d.set_item("skipped_fk_fields", PyList::empty(py))?;
        Ok(d.unbind())
    }

    /// Atomically apply a set of edge inserts and deletes in a single WAL commit.
    ///
    /// `inserts` — each `{edge_type, src, dst}` to insert (user-owned edge).
    /// `deletes` — each `{edge_type, src, dst}` to delete.
    ///
    /// All operations are committed in one fsync.  This is the efficient API
    /// for the hand-rolled maintenance pattern where a property update triggers
    /// many retractions and additions — using individual `insert_edge` /
    /// `delete_edge` calls would serialize one WAL fsync per call.
    ///
    /// Returns `{"edges_inserted": N, "edges_deleted": M}`.
    #[pyo3(
        signature = (inserts=None, deletes=None),
        text_signature = "($self, inserts=None, deletes=None)"
    )]
    fn batch_edges(
        &self,
        py: Python<'_>,
        inserts: Option<Bound<'_, PyList>>,
        deletes: Option<Bound<'_, PyList>>,
    ) -> PyResult<Py<PyDict>> {
        fn parse_edges(list: &Bound<'_, PyList>) -> PyResult<Vec<(String, String, String)>> {
            let mut ops = Vec::with_capacity(list.len());
            for item in list.iter() {
                let d = item.downcast::<PyDict>().map_err(|_| {
                    PyTypeError::new_err("each edge must be a dict {edge_type, src, dst}")
                })?;
                let edge_type: String = d
                    .get_item("edge_type")?
                    .ok_or_else(|| PyValueError::new_err("edge dict missing 'edge_type'"))?
                    .extract()?;
                let src: String = d
                    .get_item("src")?
                    .ok_or_else(|| PyValueError::new_err("edge dict missing 'src'"))?
                    .extract()?;
                let dst: String = d
                    .get_item("dst")?
                    .ok_or_else(|| PyValueError::new_err("edge dict missing 'dst'"))?
                    .extract()?;
                ops.push((edge_type, src, dst));
            }
            Ok(ops)
        }
        let insert_ops = inserts
            .as_ref()
            .map(parse_edges)
            .transpose()?
            .unwrap_or_default();
        let delete_ops = deletes
            .as_ref()
            .map(parse_edges)
            .transpose()?
            .unwrap_or_default();
        let n_insert = insert_ops.len();
        let n_delete = delete_ops.len();
        self.with_mut(|db| {
            let mut batch = db.batch();
            for (etype, src, dst) in &insert_ops {
                batch.insert_edge(etype, src, dst);
            }
            for (etype, src, dst) in &delete_ops {
                batch.delete_edge(etype, src, dst);
            }
            batch.commit().map(|_| ())
        })?;
        let d = PyDict::new(py);
        d.set_item("edges_inserted", n_insert)?;
        d.set_item("edges_deleted", n_delete)?;
        Ok(d.unbind())
    }

    /// PageRank over the whole store.
    ///
    /// Returns `{"scores": [(key, score), …], "converged": bool}`, one row
    /// per live node, highest first, ties by key.
    ///
    /// `direction` is `"out"` (rank flows along each edge, the default),
    /// `"in"` or `"both"`. `edge_type=None` follows every edge type.
    /// `weight_prop` names an edge property to weight by, and `min_weight`
    /// drops edges below it.
    ///
    /// **`budget_ms` defaults to `0` — no time limit.** The engine's own
    /// default is a 5-second wall-clock budget, under which a loaded machine
    /// returns a different answer for the same store; `0` means the same
    /// store always gives the same scores. Pass a budget to bound the call,
    /// and read `converged`.
    ///
    /// Refused on a `scoped()` handle with `ValueError`: no algorithm takes a
    /// mask, and a visible node's score would be computed over hidden edges.
    ///
    /// ```python
    /// top = db.pagerank(edge_type="CITES")["scores"][:10]
    /// ```
    #[allow(clippy::too_many_arguments)]
    #[allow(deprecated)]
    #[pyo3(
        signature = (damping = 0.85, max_iters = 50, tol = 1e-6, edge_type = None, direction = "out", budget_ms = 0, weight_prop = None, min_weight = None),
        text_signature = "($self, damping=0.85, max_iters=50, tol=1e-06, edge_type=None, direction='out', budget_ms=0, weight_prop=None, min_weight=None)"
    )]
    fn pagerank(
        &self,
        py: Python<'_>,
        damping: f64,
        max_iters: u32,
        tol: f64,
        edge_type: Option<String>,
        direction: &str,
        budget_ms: u64,
        weight_prop: Option<String>,
        min_weight: Option<f64>,
    ) -> PyResult<Py<PyDict>> {
        self.refuse_read_if_scoped("pagerank")?;
        let config = core_api::PageRankConfig {
            damping,
            max_iters,
            tol,
            edge_type,
            direction: parse_algo_dir(direction)?,
            budget_ms,
            weight_prop,
            min_weight,
        };
        let report = py.allow_threads(|| self.with_ref(|db| Ok(db.pagerank(&config))))?;
        let d = PyDict::new(py);
        d.set_item("scores", report.scores)?;
        d.set_item("converged", report.converged)?;
        Ok(d.unbind())
    }

    /// Weakly connected components.
    ///
    /// Returns `{"components": [(key, component), …], "truncated": bool}`,
    /// one row per live node. `component` is an identifier shared by every
    /// node in the same component; compare them, do not parse them.
    ///
    /// `edge_type=None` follows every edge type; direction is ignored.
    /// `budget_ms` defaults to `0` (no limit) — see `pagerank`.
    ///
    /// Refused on a `scoped()` handle with `ValueError`.
    #[allow(deprecated)]
    #[pyo3(
        signature = (edge_type = None, budget_ms = 0, weight_prop = None, min_weight = None),
        text_signature = "($self, edge_type=None, budget_ms=0, weight_prop=None, min_weight=None)"
    )]
    fn connected_components(
        &self,
        py: Python<'_>,
        edge_type: Option<String>,
        budget_ms: u64,
        weight_prop: Option<String>,
        min_weight: Option<f64>,
    ) -> PyResult<Py<PyDict>> {
        self.refuse_read_if_scoped("connected_components")?;
        let config = core_api::WccConfig {
            edge_type,
            budget_ms,
            weight_prop,
            min_weight,
        };
        let report =
            py.allow_threads(|| self.with_ref(|db| Ok(db.connected_components(&config))))?;
        let d = PyDict::new(py);
        d.set_item("components", report.components)?;
        d.set_item("truncated", report.truncated)?;
        Ok(d.unbind())
    }

    /// Degree centrality: every node's degree, highest first.
    ///
    /// Returns `{"scores": [(key, degree), …], "truncated": bool}`.
    ///
    /// **Not `degree()` or `degrees()`.** Those answer for the keys you name
    /// and can be scoped, masked and filtered; this ranks the whole store.
    /// `direction` is `"both"` (the default), `"out"` or `"in"`.
    /// `budget_ms` defaults to `0` (no limit) — see `pagerank`.
    ///
    /// Refused on a `scoped()` handle with `ValueError`; `degrees()` is the
    /// scoped way to ask.
    #[allow(deprecated)]
    #[pyo3(
        signature = (edge_type = None, direction = "both", budget_ms = 0, weight_prop = None, min_weight = None),
        text_signature = "($self, edge_type=None, direction='both', budget_ms=0, weight_prop=None, min_weight=None)"
    )]
    fn degree_centrality(
        &self,
        py: Python<'_>,
        edge_type: Option<String>,
        direction: &str,
        budget_ms: u64,
        weight_prop: Option<String>,
        min_weight: Option<f64>,
    ) -> PyResult<Py<PyDict>> {
        self.refuse_read_if_scoped("degree_centrality")?;
        let config = core_api::DegreeConfig {
            edge_type,
            direction: parse_algo_dir(direction)?,
            budget_ms,
            weight_prop,
            min_weight,
        };
        let report = py.allow_threads(|| self.with_ref(|db| Ok(db.degree_centrality(&config))))?;
        let d = PyDict::new(py);
        d.set_item("scores", report.scores)?;
        d.set_item("truncated", report.truncated)?;
        Ok(d.unbind())
    }

    /// Communities by modularity (Louvain).
    ///
    /// Returns `{"communities": [{"id", "members", "internal_weight",
    /// "cohesion"}, …], "modularity": float, "truncated": bool}`.
    ///
    /// `edge_types` is a **list** — unlike the other three algorithms, which
    /// take one `edge_type` — and `None` or `[]` follows every edge type.
    /// `node_label` restricts the pass to one label. `resolution` above 1.0
    /// favours smaller communities. `budget_ms` defaults to `0` (no limit) —
    /// see `pagerank`.
    ///
    /// Modularity is global: adding unrelated nodes can move an existing
    /// community. For "which keys are one entity", use `identity_clusters`.
    ///
    /// Refused on a `scoped()` handle with `ValueError`.
    #[allow(clippy::too_many_arguments)]
    #[allow(deprecated)]
    #[pyo3(
        signature = (edge_types = None, weight_prop = None, min_weight = None, resolution = 1.0, max_passes = 10, max_sweeps = 20, budget_ms = 0, node_label = None),
        text_signature = "($self, edge_types=None, weight_prop=None, min_weight=None, resolution=1.0, max_passes=10, max_sweeps=20, budget_ms=0, node_label=None)"
    )]
    fn communities(
        &self,
        py: Python<'_>,
        edge_types: Option<Vec<String>>,
        weight_prop: Option<String>,
        min_weight: Option<f64>,
        resolution: f64,
        max_passes: u32,
        max_sweeps: u32,
        budget_ms: u64,
        node_label: Option<String>,
    ) -> PyResult<Py<PyDict>> {
        self.refuse_read_if_scoped("communities")?;
        let config = core_api::LouvainConfig {
            edge_types: edge_types.unwrap_or_default(),
            weight_prop,
            min_weight,
            resolution,
            max_passes,
            max_sweeps,
            budget_ms,
            node_label,
        };
        let report = py.allow_threads(|| self.with_ref(|db| Ok(db.communities(&config))))?;
        let list = PyList::empty(py);
        for c in &report.communities {
            let cd = PyDict::new(py);
            cd.set_item("id", c.id)?;
            cd.set_item("members", c.members.clone())?;
            cd.set_item("internal_weight", c.internal_weight)?;
            cd.set_item("cohesion", c.cohesion)?;
            list.append(cd)?;
        }
        let d = PyDict::new(py);
        d.set_item("communities", list)?;
        d.set_item("modularity", report.modularity)?;
        d.set_item("truncated", report.truncated)?;
        Ok(d.unbind())
    }

    /// Declare a full-text index on `(label, field)`. Returns `True` when it
    /// was newly enabled.
    ///
    /// The engine's call is not idempotent: enabling a pair that is already
    /// enabled raises `RuleInvalid`. With `if_not_exists=True` it returns
    /// `False` instead, so a caller that declares its schema at boot can call
    /// this every time.
    ///
    /// The declaration is logged; the index itself is rebuilt on every open,
    /// so each declared pair adds to how long the store takes to open.
    ///
    /// ```python
    /// db.enable_fulltext("Doc", "body", if_not_exists=True)
    /// ```
    #[pyo3(
        signature = (label, field, if_not_exists = false),
        text_signature = "($self, label, field, if_not_exists=False)"
    )]
    fn enable_fulltext(&self, label: &str, field: &str, if_not_exists: bool) -> PyResult<bool> {
        // `with_mut` refuses a scoped handle before the closure runs, so the
        // `if_not_exists` read below is never reached on one. The read and
        // the write share the one guard: two threads declaring the same pair
        // at boot cannot both find it absent and have the second one raise.
        self.with_mut(|db| {
            if if_not_exists && db.is_fulltext_enabled(label, field) {
                return Ok(false);
            }
            db.enable_fulltext(label, field)?;
            Ok(true)
        })
    }

    /// Drop the full-text index on `(label, field)` and its postings.
    ///
    /// Raises `RuleNotFound` when the pair is not enabled; its `.name` is
    /// `"fulltext(label,field)"`.
    #[pyo3(text_signature = "($self, label, field)")]
    fn disable_fulltext(&self, label: &str, field: &str) -> PyResult<()> {
        self.with_mut(|db| db.disable_fulltext(label, field))
    }

    /// Whether `(label, field)` has a full-text index.
    ///
    /// A schema fact about a pair you named, so it answers on a `scoped()`
    /// handle too, as `is_index_enabled` does.
    #[pyo3(text_signature = "($self, label, field)")]
    fn is_fulltext_enabled(&self, label: &str, field: &str) -> PyResult<bool> {
        self.with_ref(|db| Ok(db.is_fulltext_enabled(label, field)))
    }

    /// Every `(label, field)` pair with a full-text index, sorted.
    ///
    /// Refused on a `scoped()` handle with `ValueError`: it enumerates labels
    /// the scope may hide. `is_fulltext_enabled` answers about one pair.
    #[pyo3(text_signature = "($self)")]
    fn fulltext_pairs(&self) -> PyResult<Vec<(String, String)>> {
        self.refuse_read_if_scoped("fulltext_pairs")?;
        self.with_ref(|db| Ok(db.fulltext_pairs()))
    }

    /// Full-text search on `field`. Returns `[(key, score), …]`, BM25,
    /// highest first, ties by key.
    ///
    /// The query grammar: space-separated terms are ANDed; `OR` between
    /// terms; `"a phrase"`; `-term` excludes; `prefix*` matches a prefix.
    ///
    /// **Keyed by field alone.** If two labels are indexed on the same field
    /// name, both are searched; filter the keys yourself.
    ///
    /// `k=0` (the default) returns every hit; a positive `k` stops at the
    /// best `k`.
    ///
    /// Refused on a `scoped()` handle with `ValueError`: the index takes no
    /// mask. `search_hybrid` is the scoped way to search text.
    #[pyo3(
        signature = (field, query, k = 0),
        text_signature = "($self, field, query, k=0)"
    )]
    fn search(&self, field: &str, query: &str, k: usize) -> PyResult<Vec<(String, f64)>> {
        self.refuse_read_if_scoped("search")?;
        self.with_ref(|db| Ok(db.search_top(field, query, k)))
    }

    /// Every rule the store holds, as a list of dicts sorted by name.
    ///
    /// Each dict is a rule definition in the shape `create_rule` accepts —
    /// `name`, `src_label`, `dst_label`, `predicate` (the externally-tagged
    /// form, `{"FieldEqual": {"field": "team"}}`), `edge_type`,
    /// `weight_prop`, `max_edges`, `approximate`, `via_label`, `via_edge`,
    /// `via_dir`, `namespace` — so a listed rule can be deleted and
    /// recreated from its own listing. Unset fields are `None`.
    ///
    /// Refused on a `scoped()` handle with `ValueError`: a rule names labels,
    /// fields and a namespace the scope may hide. `has_vector_rule` answers
    /// about one field.
    #[pyo3(text_signature = "($self)")]
    fn rules(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.refuse_read_if_scoped("rules")?;
        let mut defs = self.with_ref(|db| Ok(db.rules()))?;
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        report_to_py(py, serde_json::to_value(&defs))
    }

    /// Delete a rule and retract every edge it derived.
    ///
    /// Raises `RuleNotFound` (with `.name`) when no rule has that name.
    #[pyo3(text_signature = "($self, name)")]
    fn delete_rule(&self, name: &str) -> PyResult<()> {
        self.with_mut(|db| db.delete_rule(name))
    }

    /// Re-derive every edge of one rule from the store as it is now.
    ///
    /// The only way out of a tripped rule: `stats()` reports `tripped` when a
    /// rule hit its `max_edges` cap and stopped deriving.
    ///
    /// Raises `RuleNotFound` (with `.name`) when no rule has that name.
    #[pyo3(text_signature = "($self, name)")]
    fn rebuild_rule(&self, name: &str) -> PyResult<()> {
        self.with_mut(|db| db.rebuild_rule(name))
    }

    /// What rules the store's own data suggests. Creates nothing.
    ///
    /// Returns `{"suggestions": [...], "total": int, "bookkeeping_hidden":
    /// int, "truncated": bool}`. Each suggestion has `name`, `src_label`,
    /// `dst_label`, `edge_type`, `predicate` (one clause of text),
    /// `est_edges`, `examples` (`[src, dst, score]` lists), `rationale`, and
    /// **`create_rule_args`: pass that dict to `create_rule` unchanged.** It
    /// carries an explicit `weight_prop`, so the rule it creates here is the
    /// rule the MCP `create_rule` tool creates from the same suggestion.
    ///
    /// Proposals over fields the store writes for itself — `ns`, `kind`,
    /// `ts`, `source`, `provisional`, `id`, `aliases`, `alias_keys` — are
    /// dropped and counted in `bookkeeping_hidden`. The list is not capped
    /// (the MCP tool shows five); `truncated` means the engine's 5-second
    /// budget ran out and a second call may find more.
    ///
    /// Every proposal is a global rule: it links across namespaces.
    ///
    /// Refused on a `scoped()` handle with `ValueError`.
    #[allow(deprecated)]
    #[pyo3(text_signature = "($self)")]
    fn suggest_rules(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.refuse_read_if_scoped("suggest_rules")?;
        let found = py.allow_threads(|| {
            self.with_ref(|db| Ok(core_api::memory::suggest::filtered_suggestions(db)))
        })?;
        report_to_py(py, serde_json::to_value(&found))
    }

    /// Write a note the store can later `recall`, and whatever it names.
    ///
    /// `about` is the keys the note is about. A key that does not exist is
    /// created as a provisional `Entity` rather than refusing the call, and
    /// listed under `provisional` in the report.
    ///
    /// `entities` is a list of `{"key", "label", "props"?, "aliases"?}`
    /// dicts — entities you recognised in the text, created or updated in the
    /// same commit. `facts` is a list of `{"subject", "predicate", "object"}`
    /// dicts — relationships among them, written as edges.
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
    /// the identity links this call made, and the ones it retracted).
    ///
    /// Raises `IngestError` when the text is empty or over 4,000 characters,
    /// the `kind` is unknown, or an entity's `props` carry `aliases` or
    /// `alias_keys`; nothing is written. Its `.detail` is the bare sentence;
    /// the message carries an `ingest error: ` prefix in front of it.
    ///
    /// A store this binding creates has no memory schema. `remember`
    /// declares full-text on `Note.text` and on each new entity label's
    /// `name` itself, so `recall` works after the first call.
    ///
    /// ```python
    /// r = db.remember("Matthew is driving 0.7", about=["matthew"],
    ///                 entities=[{"key": "v0.7", "label": "Release"}],
    ///                 facts=[{"subject": "matthew", "predicate": "WORKS_ON", "object": "v0.7"}])
    /// r["note"]         # "note:…"
    /// r["provisional"]  # ["matthew"]
    /// ```
    #[allow(clippy::too_many_arguments)]
    #[pyo3(
        signature = (text, about = None, kind = None, ts = None, source = None, entities = None, facts = None),
        text_signature = "($self, text, about=None, kind=None, ts=None, source=None, entities=None, facts=None)"
    )]
    fn remember(
        &self,
        py: Python<'_>,
        text: &str,
        about: Option<Vec<String>>,
        kind: Option<&str>,
        ts: Option<i64>,
        source: Option<&str>,
        entities: Option<Bound<'_, PyList>>,
        facts: Option<Bound<'_, PyList>>,
    ) -> PyResult<Py<PyAny>> {
        use core_api::memory::remember::{
            dedup_keep_order, remember, unix_now_secs, RememberInput, DEFAULT_NOTE_KIND,
        };
        // First, so a scoped handle is refused before any argument is read.
        self.refuse_if_scoped()?;
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
        let report = self.with_mut(|db| remember(db, &input))?;
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
    fn recall(&self, py: Python<'_>, topic: &str) -> PyResult<Py<PyAny>> {
        self.refuse_read_if_scoped("recall")?;
        let rows = self.with_ref(|db| Ok(core_api::memory::recall::recall_rows(db, topic)))?;
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
    /// "same_as_lost"}`. `same_as_lost` lists the identity links this update
    /// retracted, as `{"a", "b", "score"}`: a changed `name` can take a
    /// full-name link below the floor.
    ///
    /// `aliases` or `alias_keys` inside `props` raise `IngestError`: the
    /// store maintains both. An `IngestError`'s `.detail` is the bare
    /// sentence; its message carries an `ingest error: ` prefix.
    ///
    /// ```python
    /// db.upsert_entity("ada", {"name": "Ada Lovelace"}, label="Person", aliases=["Countess"])
    /// ```
    #[pyo3(
        signature = (key, props, label = None, aliases = None, namespace = None),
        text_signature = "($self, key, props, label=None, aliases=None, namespace=None)"
    )]
    fn upsert_entity(
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
        self.refuse_if_scoped()?;
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
        let outcome = self.with_mut(|db| {
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
    /// `brief["partial"]` and the counts are then lower bounds.
    ///
    /// The MCP `schema` tool renders this same report. The names under
    /// `rules`, `fulltext`, `indexes` and `provisional_sample` are raw stored
    /// content, unsanitized, as `recall`'s rows are.
    ///
    /// Refused on a `scoped()` handle with `ValueError`.
    #[allow(deprecated)]
    #[pyo3(signature = (budget_ms = 1000), text_signature = "($self, budget_ms=1000)")]
    fn schema_report(&self, py: Python<'_>, budget_ms: u64) -> PyResult<Py<PyAny>> {
        self.refuse_read_if_scoped("schema_report")?;
        let opts = core_api::memory::brief::BriefOptions {
            budget: std::time::Duration::from_millis(budget_ms),
        };
        let report = py.allow_threads(|| {
            self.with_ref(|db| Ok(core_api::memory::schema::schema_report(db, &opts)))
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
    fn forget(
        &self,
        py: Python<'_>,
        key: Option<String>,
        prop: Option<String>,
        fact: Option<Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        use core_api::memory::forget::{forget, ForgetTarget, FORGET_SHAPE};
        self.refuse_if_scoped()?;
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
        let report = match self.with_mut(|db| Ok(forget(db, &target)))? {
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
    /// a link are one claim.
    ///
    /// Empty on a store with no `SAME_AS` edges. The identity preset that
    /// derives them is applied with `mushroomdb schema apply <db>
    /// --memory-identity`, on the command line, with this handle closed; a
    /// `SAME_AS` rule made with `create_rule` is the other way.
    ///
    /// Raises `ValueError` for a `floor` that is not a finite number.
    ///
    /// Refused on a `scoped()` handle with `ValueError`.
    #[pyo3(
        signature = (floor = core_api::memory::identity::SAME_AS_FLOOR),
        text_signature = "($self, floor=0.6)"
    )]
    fn identity_clusters(&self, py: Python<'_>, floor: f64) -> PyResult<Py<PyAny>> {
        self.refuse_read_if_scoped("identity_clusters")?;
        if !floor.is_finite() {
            return Err(PyValueError::new_err(format!(
                "identity_clusters: floor must be a finite number, got {floor}"
            )));
        }
        let report =
            self.with_ref(|db| Ok(core_api::memory::identity::identity_clusters(db, floor)))?;
        report_to_py(py, serde_json::to_value(&report))
    }

    /// Return database statistics: node/edge counts, `namespaces` (every
    /// namespace with at least one live node and its count), plus per-rule
    /// provenance size, trip latch, and fire counter.  Shape matches the HTTP
    /// `/stats` JSON response.
    #[pyo3(text_signature = "($self)")]
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        let s = self.with_scope(|db, mask| {
            let mut s = db.stats();
            if let Some(mask) = mask {
                // Store-wide counts stay store-wide, but the namespace roster
                // is node data: it names which tenants exist and how many nodes
                // each holds. A name with nothing visible in it drops out, and
                // the counts that remain are counts of visible nodes.
                s.namespaces = s
                    .namespaces
                    .iter()
                    .filter_map(|n| {
                        let visible = mask.intersect(&db.mask_for_namespace(&n.name)).len();
                        (visible > 0).then(|| NamespaceStats {
                            name: n.name.clone(),
                            nodes_live: visible,
                        })
                    })
                    .collect();
            }
            Ok(s)
        })?;
        let d = PyDict::new(py);
        d.set_item("nodes_live", s.nodes_live)?;
        d.set_item("nodes_tombstoned", s.nodes_tombstoned)?;
        d.set_item("edges", s.edges)?;
        d.set_item("history_floor", s.history_floor)?;
        let rules_list = PyList::empty(py);
        for r in &s.rules {
            let rd = PyDict::new(py);
            rd.set_item("name", &r.name)?;
            rd.set_item("edges", r.edges)?;
            rd.set_item("tripped", r.tripped)?;
            rd.set_item("fires", r.fires)?;
            rd.set_item("approximate", r.approximate)?;
            rules_list.append(rd)?;
        }
        d.set_item("rules", rules_list)?;
        let ns_list = PyList::empty(py);
        for n in &s.namespaces {
            let nd = PyDict::new(py);
            nd.set_item("name", &n.name)?;
            nd.set_item("nodes_live", n.nodes_live)?;
            ns_list.append(nd)?;
        }
        d.set_item("namespaces", ns_list)?;
        Ok(d.unbind())
    }

    /// The roles `roles.json` defines, as a list of dicts.
    ///
    /// Each dict carries `name`, `labels`, `keys`, `namespaces` and
    /// `visible_where`. `namespaces` is `None` for a role bound to no
    /// namespace — which means every one, not none — and `visible_where` is
    /// `None` or `{"field": …, "eq": …, "in": […]}`, the shape `where=` takes.
    ///
    /// `[]` when the store defines no roles. **Raises `Corrupt` when
    /// `roles.json` was corrupt at open**, which is what `scoped(role=…)` and
    /// every role-scoped read already do for the same cause. Answering `[]`
    /// there would be the dangerous answer: an unrestricted store answers `[]`
    /// too, so a poisoned sidecar would read as "nothing is restricted here".
    ///
    /// This is the call that lets a sidecar check a role name at boot instead
    /// of discovering the typo on its first request:
    ///
    /// ```python
    /// known = {r["name"] for r in db.roles()}
    /// missing = configured_roles - known        # fail the boot, not the request
    /// ```
    ///
    /// Refused on a scoped handle — see the note on `scoped()` — and the
    /// refusal is a plain **`ValueError`**, not a `MushroomError`.
    ///
    /// That is deliberate, and a refused read is the one kind of refusal in
    /// this binding that is not a typed engine error, so it is worth saying
    /// why. `ReadOnly` — the `MushroomError` subclass a refused write raises
    /// — means *a scoped handle never writes*: `refuse_if_scoped` is reached
    /// only from the write path. `roles()` is a read refused outright, as the
    /// whole-store reads are (`pagerank`, `search`, `rules` and the others
    /// that say so), so it has no `ReadOnly` precedent to follow; raising one here
    /// would make that class mean two different things. Calling `roles()` on a
    /// scoped handle is caller misuse with no engine condition behind it — the
    /// same kind of thing as `scoped()`'s empty-scope refusal, which is a
    /// `ValueError` too.
    ///
    /// The cost is real and is the reason this is documented rather than
    /// merely true: a sidecar that wraps its boot-time role check in
    /// `except mushroomdb.MushroomError` will **not** catch this one. Catch
    /// `ValueError` as well, or call `roles()` before narrowing the handle.
    #[pyo3(text_signature = "($self)")]
    fn roles(&self, py: Python<'_>) -> PyResult<Py<PyList>> {
        // The one schema surface made of node data. A role definition names the
        // node keys it grants outright, the namespaces `stats()` is at pains to
        // narrow, and the name of every other role in the store. Narrowing it is
        // no better than leaking it: a definition with its hidden keys filtered
        // out is not the definition, and a caller checking a role against a
        // doctored copy is worse off than one that was refused.
        if self.scope.is_some() {
            return Err(PyValueError::new_err(
                "roles() is refused on a scoped handle: a role definition names node keys, \
                 namespaces and the other roles in the store, and a narrowed copy would not be \
                 the definition; call it on the handle scoped() was called on",
            ));
        }
        let roles = self.with_ref(|db| db.roles_checked())?;
        let out = PyList::empty(py);
        for r in &roles {
            let d = PyDict::new(py);
            d.set_item("name", &r.name)?;
            d.set_item("labels", r.labels.clone())?;
            d.set_item("keys", r.keys.clone())?;
            match &r.namespaces {
                Some(ns) => d.set_item("namespaces", ns.clone())?,
                None => d.set_item("namespaces", py.None())?,
            }
            match &r.visible_where {
                Some(p) => d.set_item("visible_where", predicate_to_py(py, p)?)?,
                None => d.set_item("visible_where", py.None())?,
            }
            out.append(d)?;
        }
        Ok(out.unbind())
    }

    /// Write a durable snapshot and truncate the WAL tail.
    ///
    /// After `snapshot()`, the next `GraphDb.open()` on the same path loads
    /// the snapshot directly and skips WAL replay, making reopen significantly
    /// faster for large databases.
    #[pyo3(text_signature = "($self)")]
    fn snapshot(&self) -> PyResult<()> {
        self.with_mut(|db| db.snapshot())
    }

    /// Seed the store directory `dst` from the backup `src`, and say what it
    /// did.
    ///
    /// `src` is either a backup directory itself or a directory of them, in
    /// which case a subdirectory named `latest` wins outright and otherwise the
    /// newest by mtime does. The files are copied into a staging directory
    /// inside `dst` and **opened there** — the same CRC and replay checks any
    /// open runs — and only a copy that opened is moved into place, so a backup
    /// that does not open leaves `dst` exactly as it was found.
    ///
    /// Returns a dict with `outcome`, one of:
    ///
    /// - `"restored"` — seeded, with `from`, `files` and `bytes` filled in;
    /// - `"already_present"` — `dst` already holds a store. **It is refused,
    ///   not merged**: nothing of the backup is copied over a live store;
    /// - `"empty"` — nothing under `src` looks like a store. A first boot on an
    ///   empty backup volume is a report, not a failure.
    ///
    /// So a caller that requires a fresh restore must read `outcome`; a sidecar
    /// rebuilding on boot can ignore it and call this every time, which is what
    /// `serve --restore-from` does.
    ///
    /// Raises `IoError` when a copy, an install or the staged open failed; the
    /// message names both directories.
    ///
    /// ```python
    /// GraphDb.restore("/backups", "/data/db")
    /// db = GraphDb.open("/data/db")
    /// ```
    #[staticmethod]
    #[pyo3(text_signature = "(src, dst)")]
    fn restore(py: Python<'_>, src: PathBuf, dst: PathBuf) -> PyResult<Py<PyDict>> {
        // Argument order is the caller's — source first, destination second —
        // while the engine takes the destination first. Named bindings, so the
        // pair cannot be swapped by editing one line.
        let outcome = restore_if_empty(&dst, &src).map_err(graph_err)?;
        let d = PyDict::new(py);
        match outcome {
            RestoreOutcome::Restored { from, files, bytes } => {
                d.set_item("outcome", "restored")?;
                d.set_item("from", from.to_string_lossy().as_ref())?;
                d.set_item("files", files)?;
                d.set_item("bytes", bytes)?;
            }
            RestoreOutcome::AlreadyPresent => {
                d.set_item("outcome", "already_present")?;
                d.set_item("from", py.None())?;
                d.set_item("files", Vec::<String>::new())?;
                d.set_item("bytes", 0u64)?;
            }
            RestoreOutcome::Empty => {
                d.set_item("outcome", "empty")?;
                d.set_item("from", py.None())?;
                d.set_item("files", Vec::<String>::new())?;
                d.set_item("bytes", 0u64)?;
            }
        }
        Ok(d.unbind())
    }

    /// Close the handle and release the store.  Further calls raise
    /// `RuntimeError`.  `GraphDb` is also a context manager, so
    /// `with GraphDb.open(path) as db:` closes on exit.
    ///
    /// A handle from `scoped()` shares the one store, so closing either name
    /// closes it for both. Closing is not a write, so a scoped handle may.
    #[pyo3(text_signature = "($self)")]
    fn close(&self) -> PyResult<()> {
        let mut guard = lock(&self.inner.0)?;
        *guard = None;
        Ok(())
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __exit__(
        &self,
        _ty: Bound<'_, PyAny>,
        _val: Bound<'_, PyAny>,
        _tb: Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        self.close()?;
        Ok(false)
    }
}

impl GraphDb {
    /// Every mutation goes through here, so the scoped refusal is one check
    /// rather than one per method — a write surface added later is refused
    /// without anyone remembering to refuse it.
    ///
    /// `refresh()` is the one caller that wants the `&mut Db` without the
    /// refusal, and it uses [`GraphDb::with_mut_unscoped`] directly.
    fn with_mut<T, F>(&self, f: F) -> PyResult<T>
    where
        F: FnOnce(&mut Db) -> core_api::Result<T>,
    {
        self.refuse_if_scoped()?;
        self.with_mut_unscoped(f)
    }

    fn with_mut_unscoped<T, F>(&self, f: F) -> PyResult<T>
    where
        F: FnOnce(&mut Db) -> core_api::Result<T>,
    {
        let mut guard = lock(&self.inner.0)?;
        let out = {
            let db = guard
                .as_mut()
                .ok_or_else(|| PyRuntimeError::new_err("GraphDb is closed"))?;
            f(db)
        };
        // Release the store before `graph_err`, which takes the GIL. See
        // [`GraphDb::with_scope`] for the inversion that rule prevents.
        drop(guard);
        out.map_err(graph_err)
    }

    /// Refuse the call when this handle came from `scoped()`.
    ///
    /// Raises `ReadOnly`, the same class an as-of instance's refused write
    /// raises: both say "this handle never writes". The message is unchanged,
    /// and `ReadOnly` is a `RuntimeError`, so every existing `except` still
    /// catches it.
    fn refuse_if_scoped(&self) -> PyResult<()> {
        if self.scope.is_some() {
            return Err(ReadOnly::new_err(
                "this handle is scoped, and a scoped handle never writes; call this on the \
                 handle scoped() was called on",
            ));
        }
        Ok(())
    }

    /// Refuse a read that answers about the whole store.
    ///
    /// No algorithm, index search, suggestion or memory report in the engine
    /// takes a mask, and filtering their rows afterwards is unsound: a visible
    /// node's score is computed over hidden topology, and an enumeration
    /// names labels a scoped caller may not see. So each is refused outright
    /// on a handle from `scoped()`.
    ///
    /// A plain `ValueError`, as `roles()` raises and for its reason:
    /// `ReadOnly` means "a scoped handle never writes", and these are reads.
    fn refuse_read_if_scoped(&self, what: &str) -> PyResult<()> {
        if self.scope.is_some() {
            return Err(PyValueError::new_err(format!(
                "{what}() is refused on a scoped handle: it answers about the whole store and \
                 no scope can narrow it honestly; call it on the handle scoped() was called on"
            )));
        }
        Ok(())
    }

    fn with_ref<T, F>(&self, f: F) -> PyResult<T>
    where
        F: FnOnce(&Db) -> core_api::Result<T>,
    {
        let guard = lock(&self.inner.0)?;
        let out = {
            let db = guard
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("GraphDb is closed"))?;
            f(db)
        };
        // Release the store before `graph_err`, which takes the GIL. See
        // [`GraphDb::with_scope`] for the inversion that rule prevents.
        drop(guard);
        out.map_err(graph_err)
    }

    /// Run a read with this handle's scope resolved for **this** read.
    ///
    /// `f` gets `None` on an unscoped handle and `Some(mask)` on a scoped one,
    /// so each method spells out its own scoped contract beside its unscoped
    /// one. Resolution happens inside the lock, against the store the read is
    /// about to run on, which is what keeps the allow-list from going stale
    /// across a write.
    ///
    /// **The guard is dropped before any `GraphError` is mapped.** `graph_err`
    /// calls `Python::attach`, and `find_similar`, `pairwise_similar`, `degree`
    /// and `degrees` call this from inside `py.allow_threads` — so mapping
    /// under the guard would take Mutex → GIL, against the GIL → Mutex every
    /// other `#[pymethods]` fn takes. Both orders on the one `Arc<Inner>` every
    /// `scoped()` child shares is a hang, not a slow read. Resolution and the
    /// read therefore share one `map_err` *after* the drop, rather than the
    /// resolve arm keeping an early return of its own that the next edit could
    /// forget to move.
    /// Resolve a "when" argument that may be a frame index or a date string.
    ///
    /// One helper so `edges_at` and `was_linked` cannot drift in what they
    /// accept. An `int` is a 0-based frame index and passes through; a `str` is
    /// parsed as RFC 3339 and resolved to the last commit at or before it.
    /// Anything else is a `ValueError` — never a guessed commit, which is the
    /// failure mode the whole date surface exists to remove.
    ///
    /// A `bool` is rejected deliberately: Python's `bool` is an `int`, so
    /// `edges_at(k, True)` would otherwise silently mean frame 1.
    fn resolve_when(&self, at: &Bound<'_, PyAny>, arg: &str) -> PyResult<u64> {
        if at.is_instance_of::<pyo3::types::PyBool>() {
            return Err(PyValueError::new_err(format!(
                "{arg} must be a frame index or an RFC 3339 date string, not a bool"
            )));
        }
        if let Ok(n) = at.extract::<u64>() {
            return Ok(n);
        }
        if let Ok(s) = at.extract::<String>() {
            return self.with_scope(|db, _| db.resolve_date(&s));
        }
        Err(PyValueError::new_err(format!(
            "{arg} must be a 0-based frame index (int) or an RFC 3339 date string \
             (\"2026-06-19\", \"2026-06-19T12:00:00Z\")"
        )))
    }

    fn with_scope<T, F>(&self, f: F) -> PyResult<T>
    where
        F: FnOnce(&Db, Option<&NodeMask>) -> core_api::Result<T>,
    {
        let guard = lock(&self.inner.0)?;
        let out = {
            let db = guard
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("GraphDb is closed"))?;
            match &self.scope {
                Some(scope) => scope.resolve(db).and_then(|mask| f(db, Some(&mask))),
                None => f(db, None),
            }
        };
        drop(guard);
        out.map_err(graph_err)
    }

    /// This handle's scope narrowed by a per-call `role=` / `namespace=` pair.
    ///
    /// Used by the time-travel read, where the legs must be resolved against
    /// the as-of graph rather than the live one, so a resolved mask is no use.
    fn scope_narrowed_by(
        &self,
        role: Option<&str>,
        namespace: Option<&str>,
    ) -> PyResult<Option<Scope>> {
        let Some(handle) = self.scope.as_ref() else {
            return Ok(None);
        };
        if role.is_none() && namespace.is_none() {
            return Ok(Some(handle.clone()));
        }
        let call = Scope::new(role.map(str::to_owned), namespace.map(str::to_owned), None)
            .map_err(graph_err)?;
        Ok(Some(handle.intersect(&call)))
    }
}

/// The scope's mask narrowed by a per-call one, when either is present.
///
/// The intersection is the never-widen rule: a per-call `mask=` can only take
/// nodes away from what the handle already allows.
fn narrow(scope: Option<&NodeMask>, call: Option<NodeMask>) -> Option<NodeMask> {
    match (scope, call) {
        (Some(s), Some(c)) => Some(s.intersect(&c)),
        (Some(s), None) => Some(s.clone()),
        (None, c) => c,
    }
}

/// Convert a [`HistoryEntry`] into a Python dict tagged by `kind`.
fn history_entry_to_dict(py: Python<'_>, e: &HistoryEntry) -> PyResult<Py<PyDict>> {
    let d = PyDict::new(py);
    d.set_item("commit", e.commit)?;
    match &e.change {
        HistoryChange::NodeInserted { label } => {
            d.set_item("kind", "node_inserted")?;
            d.set_item("label", label)?;
        }
        HistoryChange::PropSet { field, value } => {
            d.set_item("kind", "prop_set")?;
            d.set_item("field", field)?;
            d.set_item("value", value_to_py(py, value)?)?;
        }
        HistoryChange::PropRemoved { field } => {
            d.set_item("kind", "prop_removed")?;
            d.set_item("field", field)?;
        }
        HistoryChange::EdgeAdded {
            edge_type,
            other,
            outgoing,
        } => {
            d.set_item("kind", "edge_added")?;
            d.set_item("edge_type", edge_type)?;
            d.set_item("other", other)?;
            d.set_item("outgoing", *outgoing)?;
        }
        HistoryChange::EdgeRemoved {
            edge_type,
            other,
            outgoing,
        } => {
            d.set_item("kind", "edge_removed")?;
            d.set_item("edge_type", edge_type)?;
            d.set_item("other", other)?;
            d.set_item("outgoing", *outgoing)?;
        }
        HistoryChange::NodeDeleted => {
            d.set_item("kind", "node_deleted")?;
        }
    }
    Ok(d.unbind())
}

fn pylist_to_f64_vec(list: &Bound<'_, PyList>) -> PyResult<Vec<f64>> {
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        // PyBool must be checked before PyInt: Python bool is a subclass of
        // int, so is_instance_of::<PyInt>() returns true for True/False too.
        // Booleans are explicitly rejected — a True/False in a query vector
        // is almost certainly a caller bug, not intentional numeric embedding.
        if item.is_instance_of::<PyBool>() {
            return Err(PyTypeError::new_err(
                "vector elements must be numbers (int or float), not bool",
            ));
        } else if item.is_instance_of::<PyInt>() {
            let i: i64 = item.extract()?;
            out.push(i as f64);
        } else if item.is_instance_of::<PyFloat>() {
            out.push(item.extract()?);
        } else {
            return Err(PyTypeError::new_err("vector elements must be int or float"));
        }
    }
    Ok(out)
}

fn lock<T>(m: &Mutex<T>) -> PyResult<std::sync::MutexGuard<'_, T>> {
    m.lock()
        .map_err(|_| PyRuntimeError::new_err("GraphDb lock poisoned"))
}

/// Hang the failing variant's own fields on the exception instance.
///
/// `setattr` on a fresh exception instance can only fail on allocation
/// failure; if it ever does, that error is what the caller sees.
fn err_with<'py>(
    py: Python<'py>,
    err: PyErr,
    set: impl FnOnce(&Bound<'py, PyBaseException>) -> PyResult<()>,
) -> PyErr {
    match set(err.value(py)) {
        Ok(()) => err,
        Err(fail) => fail,
    }
}

/// Map an engine error onto its Python class.
///
/// One class per `GraphError` variant, each a `MushroomError` and so a
/// `RuntimeError`. The message is the one the engine has always produced, so
/// existing logs and substring checks do not change; the fields the variant
/// carries become attributes, so nobody has to parse that message.
fn graph_err(e: GraphError) -> PyErr {
    Python::attach(|py| {
        let msg = match &e {
            // A Python caller has never seen the `query error: ` prefix.
            GraphError::QueryError { detail } => detail.clone(),
            other => other.to_string(),
        };
        match &e {
            GraphError::KeyNotFound { key } => err_with(py, KeyNotFound::new_err(msg), |v| {
                v.setattr("key", key.as_str())
            }),
            GraphError::DuplicateKey { key } => err_with(py, DuplicateKey::new_err(msg), |v| {
                v.setattr("key", key.as_str())
            }),
            // The variant's one field is an unnamed `std::io::Error`, whose
            // own text is already the message.
            GraphError::Io(_) => IoError::new_err(msg),
            GraphError::Corrupt { detail } => err_with(py, Corrupt::new_err(msg), |v| {
                v.setattr("detail", detail.as_str())
            }),
            GraphError::RuleInvalid { detail } => err_with(py, RuleInvalid::new_err(msg), |v| {
                v.setattr("detail", detail.as_str())
            }),
            GraphError::RuleOwned { detail } => err_with(py, RuleOwned::new_err(msg), |v| {
                v.setattr("detail", detail.as_str())
            }),
            GraphError::RuleNotFound { name } => err_with(py, RuleNotFound::new_err(msg), |v| {
                v.setattr("name", name.as_str())
            }),
            GraphError::QueryError { detail } => err_with(py, QueryError::new_err(msg), |v| {
                v.setattr("detail", detail.as_str())
            }),
            GraphError::IngestError { detail } => err_with(py, IngestError::new_err(msg), |v| {
                v.setattr("detail", detail.as_str())
            }),
            GraphError::ReadOnly => ReadOnly::new_err(msg),
            GraphError::CommitOutOfRange {
                commit,
                total,
                floor,
            } => err_with(py, CommitOutOfRange::new_err(msg), |v| {
                v.setattr("commit", *commit)?;
                v.setattr("total", *total)?;
                v.setattr("floor", *floor)
            }),
            GraphError::ViewPropReadOnly { view_name } => {
                err_with(py, ViewPropReadOnly::new_err(msg), |v| {
                    v.setattr("view_name", view_name.as_str())
                })
            }
            GraphError::CasConflict {
                key,
                expected,
                actual,
            } => err_with(py, CasConflict::new_err(msg), |v| {
                v.setattr("key", key.as_str())?;
                v.setattr("expected", *expected)?;
                v.setattr("actual", *actual)
            }),
            GraphError::MaskedReadOnly => MaskedReadOnly::new_err(msg),
            GraphError::RoleWriteDenied { reason } => {
                err_with(py, RoleWriteDenied::new_err(msg), |v| {
                    v.setattr("reason", reason.as_str())
                })
            }
            // Busy is the one error a caller is expected to catch and retry,
            // and matching on a message is not an API.
            GraphError::Busy { holder } => err_with(py, MushroomBusy::new_err(msg), |v| {
                v.setattr("holder", *holder)
            }),
            GraphError::NamespaceImmutable { key, from, to } => {
                err_with(py, NamespaceImmutable::new_err(msg), |v| {
                    v.setattr("key", key.as_str())?;
                    // `from` is a Python keyword, so it cannot be an attribute.
                    v.setattr("from_", from.as_str())?;
                    v.setattr("to", to.as_str())
                })
            }
            GraphError::CrossNamespace {
                src,
                src_ns,
                dst,
                dst_ns,
            } => err_with(py, CrossNamespace::new_err(msg), |v| {
                v.setattr("src", src.as_str())?;
                v.setattr("src_ns", src_ns.as_str())?;
                v.setattr("dst", dst.as_str())?;
                v.setattr("dst_ns", dst_ns.as_str())
            }),
            GraphError::CommitTimeNotMonotonic {
                supplied_ms,
                newest_ms,
            } => err_with(py, CommitTimeNotMonotonic::new_err(msg), |v| {
                v.setattr("supplied_ms", *supplied_ms)?;
                v.setattr("newest_ms", *newest_ms)
            }),
            GraphError::NoRecordedTime => NoRecordedTime::new_err(msg),
            GraphError::TimeBeforeFloor {
                floor_ms,
                floor_commit,
            } => err_with(py, TimeBeforeFloor::new_err(msg), |v| {
                v.setattr("floor_ms", *floor_ms)?;
                v.setattr("floor_commit", *floor_commit)
            }),
        }
    })
}

fn parse_dir(s: &str) -> PyResult<Direction> {
    match s.to_ascii_lowercase().as_str() {
        "out" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        _ => Err(PyValueError::new_err("direction must be 'out' or 'in'")),
    }
}

fn parse_algo_dir(s: &str) -> PyResult<AlgoDir> {
    match s.to_ascii_lowercase().as_str() {
        "out" => Ok(AlgoDir::Out),
        "in" => Ok(AlgoDir::In),
        "both" => Ok(AlgoDir::Both),
        _ => Err(PyValueError::new_err(
            "direction must be 'out', 'in', or 'both'",
        )),
    }
}

/// A [`PropPredicate`] as the dict `py_to_where` would read back.
///
/// Both legs are always present, `None` when unset, so a caller can index the
/// dict without asking which form the role was written in.
fn predicate_to_py(py: Python<'_>, p: &PropPredicate) -> PyResult<Py<PyDict>> {
    let d = PyDict::new(py);
    d.set_item("field", &p.field)?;
    match &p.eq {
        Some(v) => d.set_item("eq", value_to_py(py, v)?)?,
        None => d.set_item("eq", py.None())?,
    }
    match &p.in_ {
        Some(values) => {
            let list = PyList::empty(py);
            for v in values {
                list.append(value_to_py(py, v)?)?;
            }
            d.set_item("in", list)?;
        }
        None => d.set_item("in", py.None())?,
    }
    Ok(d.unbind())
}

fn py_to_where(dict: &Bound<'_, PyDict>) -> PyResult<PropPredicate> {
    let field = match dict.get_item("field")? {
        Some(v) => v
            .extract::<String>()
            .map_err(|_| PyTypeError::new_err("where.field must be a string"))?,
        None => String::new(),
    };
    let eq = match dict.get_item("eq")? {
        Some(v) => Some(py_to_value(&v)?),
        None => None,
    };
    let in_ = match dict.get_item("in")? {
        Some(v) => {
            let list = v
                .downcast::<PyList>()
                .map_err(|_| PyTypeError::new_err("where.in must be a list"))?;
            let mut out = Vec::with_capacity(list.len());
            for item in list.iter() {
                out.push(py_to_value(&item)?);
            }
            Some(out)
        }
        None => None,
    };
    let pred = PropPredicate { field, eq, in_ };
    pred.validate_named("where")
        .map_err(PyValueError::new_err)?;
    Ok(pred)
}

fn py_to_value(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_instance_of::<PyBool>() {
        return Ok(Value::Bool(obj.extract()?));
    }
    if obj.is_instance_of::<PyInt>() {
        let i: i64 = obj
            .extract()
            .map_err(|_| PyTypeError::new_err("int does not fit in i64"))?;
        return Ok(Value::Int(i));
    }
    if obj.is_instance_of::<PyFloat>() {
        return Ok(Value::Float(obj.extract()?));
    }
    if obj.is_instance_of::<PyString>() {
        return Ok(Value::Str(obj.extract()?));
    }
    if let Ok(list) = obj.downcast::<PyList>() {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            out.push(py_to_value(&item)?);
        }
        return Ok(Value::List(out));
    }
    if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut map = BTreeMap::new();
        for (k, v) in dict.iter() {
            let key: String = k.extract().map_err(|_| {
                PyTypeError::new_err("dict keys must be str to convert to Value::Map")
            })?;
            map.insert(key, py_to_value(&v)?);
        }
        return Ok(Value::Map(map));
    }
    Err(PyTypeError::new_err(format!(
        "cannot convert {} to Value (need str, int, float, bool, list, or dict)",
        obj.get_type().name()?
    )))
}

fn value_to_py<'py>(py: Python<'py>, v: &Value) -> PyResult<Bound<'py, PyAny>> {
    match v {
        Value::Int(i) => Ok((*i).into_pyobject(py)?.into_any()),
        Value::Float(f) => Ok((*f).into_pyobject(py)?.into_any()),
        Value::Str(s) => Ok(s.into_pyobject(py)?.into_any()),
        Value::Bool(b) => Ok(PyBool::new(py, *b).to_owned().into_any()),
        Value::List(xs) => {
            let list = PyList::empty(py);
            for x in xs {
                list.append(value_to_py(py, x)?)?;
            }
            Ok(list.into_any())
        }
        Value::Map(m) => {
            let dict = PyDict::new(py);
            for (k, v) in m {
                dict.set_item(k, value_to_py(py, v)?)?;
            }
            Ok(dict.into_any())
        }
    }
}

/// A `serde_json::Value` as the object Python's `json.loads` would build:
/// `None`, `bool`, `int`, `float`, `str`, `list`, `dict`.
///
/// For reports that are `Serialize` in `core-api` — a rule definition, a
/// schema report, an identity report. A JSON array is a `list`, so a Rust
/// tuple inside such a report arrives as a two- or three-item list.
fn json_to_py<'py>(py: Python<'py>, v: &serde_json::Value) -> PyResult<Bound<'py, PyAny>> {
    use serde_json::Value as J;
    Ok(match v {
        J::Null => py.None().into_bound(py),
        J::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_pyobject(py)?.into_any()
            } else if let Some(u) = n.as_u64() {
                u.into_pyobject(py)?.into_any()
            } else {
                n.as_f64().unwrap_or(f64::NAN).into_pyobject(py)?.into_any()
            }
        }
        J::String(s) => s.into_pyobject(py)?.into_any(),
        J::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(json_to_py(py, item)?)?;
            }
            list.into_any()
        }
        J::Object(map) => {
            let dict = PyDict::new(py);
            for (k, item) in map {
                dict.set_item(k, json_to_py(py, item)?)?;
            }
            dict.into_any()
        }
    })
}

/// A serialised `core-api` report as a Python object. Takes the result of
/// `serde_json::to_value` so no call site needs to name a serde trait — the
/// binding depends on `serde_json`, not on `serde`.
fn report_to_py(py: Python<'_>, v: serde_json::Result<serde_json::Value>) -> PyResult<Py<PyAny>> {
    let v = v.map_err(|e| PyRuntimeError::new_err(format!("could not serialise report: {e}")))?;
    Ok(json_to_py(py, &v)?.unbind())
}

/// Convert an optional Python params argument to a `BTreeMap`.
///
/// Accepts `None` (empty map), a `dict` (name→value), or a list of
/// `(name, value)` tuples.
fn params_to_map(params: Option<Bound<'_, PyAny>>) -> PyResult<BTreeMap<String, Value>> {
    let Some(obj) = params else {
        return Ok(BTreeMap::new());
    };
    // Dict form: {"key": value, ...}
    if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut map = BTreeMap::new();
        for (k, v) in dict.iter() {
            let name: String = k.extract().map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err("params dict keys must be str")
            })?;
            map.insert(name, py_to_value(&v)?);
        }
        return Ok(map);
    }
    // Tuple-list form: [("key", value), ...]
    if let Ok(list) = obj.downcast::<PyList>() {
        let mut map = BTreeMap::new();
        for item in list.iter() {
            let tuple = item.downcast::<pyo3::types::PyTuple>().map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err(
                    "params must be a dict or a list of (name, value) tuples",
                )
            })?;
            if tuple.len() != 2 {
                return Err(pyo3::exceptions::PyTypeError::new_err(
                    "each param tuple must have exactly 2 elements",
                ));
            }
            let name: String = tuple.get_item(0)?.extract()?;
            let val = py_to_value(&tuple.get_item(1)?)?;
            map.insert(name, val);
        }
        return Ok(map);
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "params must be a dict or a list of (name, value) tuples",
    ))
}

/// A `namespace=` keyword, refused here rather than resolved to an empty mask: a
/// typo that silently answers "nothing" reads like an empty store.
fn check_namespace(namespace: Option<&str>) -> PyResult<Option<&str>> {
    match namespace {
        Some(ns) if !valid_namespace(ns) => Err(PyValueError::new_err(format!(
            "namespace {ns:?} is not a valid namespace name — 1 to {NS_MAX_LEN} characters of \
             [A-Za-z0-9_.-]"
        ))),
        other => Ok(other),
    }
}

/// `ingest_batch(on_conflict=)` — the spelling is part of the API, so an
/// unknown one is a `ValueError` rather than a silent fall-back to `"error"`.
fn parse_on_conflict(name: &str) -> PyResult<OnConflict> {
    match name {
        "error" => Ok(OnConflict::Error),
        "skip" => Ok(OnConflict::Skip),
        "replace" => Ok(OnConflict::Replace),
        other => Err(PyValueError::new_err(format!(
            "on_conflict must be \"error\", \"skip\" or \"replace\", got {other:?}"
        ))),
    }
}

fn dict_to_props(props: &Bound<'_, PyDict>) -> PyResult<Vec<(String, Value)>> {
    let mut out = Vec::with_capacity(props.len());
    for (k, v) in props.iter() {
        let key: String = k.extract()?;
        out.push((key, py_to_value(&v)?));
    }
    Ok(out)
}

/// A required string entry of a dict argument. `what` names the argument in
/// the message: `entities[]`, `facts[]`, `fact`.
fn required_str(d: &Bound<'_, PyDict>, name: &str, what: &str) -> PyResult<String> {
    match d.get_item(name)? {
        Some(v) => v
            .extract::<String>()
            .map_err(|_| PyTypeError::new_err(format!("{what}.{name} must be a str"))),
        None => Err(PyValueError::new_err(format!("{what}.{name} is required"))),
    }
}

/// `remember(entities=[{"key", "label", "props"?, "aliases"?}, …])`.
fn entities_from_py(
    list: &Bound<'_, PyList>,
) -> PyResult<Vec<core_api::memory::remember::EntityIn>> {
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        let d = item
            .downcast::<PyDict>()
            .map_err(|_| PyTypeError::new_err("entities must be a list of dicts"))?;
        let props: BTreeMap<String, Value> = match d.get_item("props")? {
            Some(p) if !p.is_none() => {
                let pd = p
                    .downcast::<PyDict>()
                    .map_err(|_| PyTypeError::new_err("entities[].props must be a dict"))?;
                dict_to_props(pd)?.into_iter().collect()
            }
            _ => BTreeMap::new(),
        };
        let aliases: Vec<String> = match d.get_item("aliases")? {
            Some(a) if !a.is_none() => a
                .extract()
                .map_err(|_| PyTypeError::new_err("entities[].aliases must be a list of str"))?,
            _ => Vec::new(),
        };
        out.push(core_api::memory::remember::EntityIn {
            key: required_str(d, "key", "entities[]")?,
            label: required_str(d, "label", "entities[]")?,
            props,
            aliases,
        });
    }
    Ok(out)
}

/// `remember(facts=[{"subject", "predicate", "object"}, …])`.
fn facts_from_py(list: &Bound<'_, PyList>) -> PyResult<Vec<core_api::memory::remember::FactIn>> {
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        let d = item
            .downcast::<PyDict>()
            .map_err(|_| PyTypeError::new_err("facts must be a list of dicts"))?;
        out.push(core_api::memory::remember::FactIn {
            subject: required_str(d, "subject", "facts[]")?,
            predicate: required_str(d, "predicate", "facts[]")?,
            object: required_str(d, "object", "facts[]")?,
        });
    }
    Ok(out)
}

fn rule_from_py(py: Python<'_>, rule: &Bound<'_, PyAny>) -> PyResult<RuleDef> {
    let missing_max_edges = match rule.downcast::<PyDict>() {
        Ok(d) => !d.contains("max_edges")?,
        Err(_) => false,
    };
    let json = py.import("json")?;
    let s: String = json.call_method1("dumps", (rule,))?.extract()?;
    let mut raw: serde_json::Value = serde_json::from_str(&s)
        .map_err(|e| PyValueError::new_err(format!("create_rule rule is not JSON-able: {e}")))?;
    if let Some(pred) = raw.get_mut("predicate") {
        *pred = normalize_predicate(pred).map_err(PyValueError::new_err)?;
    }
    let mut def: RuleDef = serde_json::from_value(raw).map_err(|e| {
        PyValueError::new_err(format!("create_rule JSON does not match RuleDef: {e}"))
    })?;
    if missing_max_edges {
        def.max_edges = Some(default_max_edges(&def.predicate));
    }
    Ok(def)
}

/// Rewrite the snake_case `PredicateSummary` shape that `explain` emits into
/// the externally-tagged shape `Predicate` deserializes from.
///
/// A predicate dict carrying a `"kind"` string is treated as the summary
/// shape; anything else is passed through untouched so the Rust-native form
/// (`{"FieldEqual": {"field": …}}`) keeps working.  This is what makes an
/// explanation round-trip straight back into `create_rule`.
fn normalize_predicate(v: &serde_json::Value) -> Result<serde_json::Value, String> {
    use serde_json::{json, Value as J};

    let Some(obj) = v.as_object() else {
        return Ok(v.clone());
    };
    let Some(J::String(kind)) = obj.get("kind") else {
        return Ok(v.clone());
    };

    // `fields` is the summary shape; `field` is tolerated for hand-written dicts.
    let field = || -> Result<String, String> {
        if let Some(J::String(f)) = obj.get("field") {
            return Ok(f.clone());
        }
        match obj
            .get("fields")
            .and_then(J::as_array)
            .and_then(|a| a.first())
        {
            Some(J::String(f)) => Ok(f.clone()),
            _ => Err(format!(
                "predicate kind `{kind}` requires a non-empty `fields` list (or a `field` string)"
            )),
        }
    };
    let number = |name: &str| -> Result<f64, String> {
        obj.get(name).and_then(J::as_f64).ok_or_else(|| {
            format!(
                "predicate kind `{kind}` requires a numeric `{name}` (got {:?})",
                obj.get(name)
            )
        })
    };
    let parts = || -> Result<Vec<J>, String> {
        let Some(J::Array(items)) = obj.get("parts") else {
            return Err(format!(
                "predicate kind `{kind}` requires a non-empty `parts` list"
            ));
        };
        if items.is_empty() {
            return Err(format!(
                "predicate kind `{kind}` requires a non-empty `parts` list"
            ));
        }
        items.iter().map(normalize_predicate).collect()
    };

    Ok(match kind.as_str() {
        "key_match" => json!({ "KeyMatch": { "field": field()? } }),
        "field_equal" => json!({ "FieldEqual": { "field": field()? } }),
        "overlap" => json!({ "Overlap": { "field": field()?, "min": number("min")? } }),
        "numeric_within" => {
            json!({ "NumericWithin": { "field": field()?, "tolerance": number("tolerance")? } })
        }
        "geo_radius" => json!({ "GeoRadius": { "field": field()?, "km": number("km")? } }),
        "vector_similar" => {
            json!({ "VectorSimilar": { "field": field()?, "min": number("min")? } })
        }
        "all" => json!({ "All": parts()? }),
        "any" => json!({ "Any": parts()? }),
        other => {
            return Err(format!(
                "unknown predicate kind `{other}`; expected one of key_match, field_equal, \
                 overlap, numeric_within, geo_radius, vector_similar, all, any"
            ))
        }
    })
}

fn result_set_to_rows(py: Python<'_>, rs: &ResultSet) -> PyResult<Vec<Py<PyDict>>> {
    let cols = rs.columns();
    let mut rows = Vec::with_capacity(rs.len());
    for i in 0..rs.len() {
        let dict = PyDict::new(py);
        for (j, col) in cols.iter().enumerate() {
            let cell = rs.row(i).get(j).and_then(|c| c.as_ref());
            match cell {
                Some(v) => dict.set_item(col, value_to_py(py, v)?)?,
                None => dict.set_item(col, py.None())?,
            }
        }
        rows.push(dict.unbind());
    }
    Ok(rows)
}

fn node_info_to_py(py: Python<'_>, info: &NodeInfo) -> PyResult<Py<PyDict>> {
    let props = PyDict::new(py);
    for (k, v) in &info.props {
        props.set_item(k, value_to_py(py, v)?)?;
    }
    let d = PyDict::new(py);
    d.set_item("key", &info.key)?;
    d.set_item("label", &info.label)?;
    d.set_item("props", props)?;
    Ok(d.unbind())
}

/// `{edge_type, src, dst, derived, rule}` — the shape `edges_at` and
/// `what_if_set_prop` return. `src`/`dst` (not `src_key`/`dst_key`) so a row
/// reads the same way as an `ingest_json` edge.
fn edge_at_to_py(py: Python<'_>, e: &EdgeAt) -> PyResult<Py<PyDict>> {
    let d = PyDict::new(py);
    d.set_item("edge_type", &e.edge_type)?;
    d.set_item("src", &e.src_key)?;
    d.set_item("dst", &e.dst_key)?;
    d.set_item("derived", e.derived)?;
    d.set_item("rule", e.rule.as_deref())?;
    Ok(d.unbind())
}

fn explanation_to_py(py: Python<'_>, e: &Explanation) -> PyResult<Py<PyDict>> {
    let d = PyDict::new(py);
    d.set_item("rule", &e.rule)?;
    d.set_item("edge_type", &e.edge_type)?;
    d.set_item("src_key", &e.src_key)?;
    d.set_item("dst_key", &e.dst_key)?;
    match e.weight {
        Some(w) => d.set_item("weight", w)?,
        None => d.set_item("weight", py.None())?,
    }
    d.set_item("predicate", summary_to_py(py, &e.predicate)?)?;
    Ok(d.unbind())
}

fn summary_to_py(py: Python<'_>, s: &PredicateSummary) -> PyResult<Py<PyDict>> {
    let d = PyDict::new(py);
    d.set_item("kind", &s.kind)?;
    d.set_item("fields", s.fields.clone())?;
    match s.min {
        Some(v) => d.set_item("min", v)?,
        None => d.set_item("min", py.None())?,
    }
    match s.tolerance {
        Some(v) => d.set_item("tolerance", v)?,
        None => d.set_item("tolerance", py.None())?,
    }
    match s.km {
        Some(v) => d.set_item("km", v)?,
        None => d.set_item("km", py.None())?,
    }
    match &s.parts {
        Some(parts) => {
            let list = PyList::empty(py);
            for p in parts {
                list.append(summary_to_py(py, p)?)?;
            }
            d.set_item("parts", list)?;
        }
        None => d.set_item("parts", py.None())?,
    }
    Ok(d.unbind())
}

// ---------------------------------------------------------------------------
// Error classes — one per `GraphError` variant (v0.6.10 §5.7)
// ---------------------------------------------------------------------------

pyo3::create_exception!(
    mushroomdb,
    MushroomError,
    PyRuntimeError,
    "Base class for every error the engine raises.\n\n\
     It subclasses `RuntimeError`, so every `except RuntimeError` written \
     against an earlier release keeps catching exactly what it caught.\n\n\
     Each subclass carries `.code`, a stable snake_case string, and the \
     failing variant's own fields as attributes. `.code` is the compatibility \
     surface: classes may be added, a code is never respelled. `str(e)` is \
     the message the engine has always produced."
);

/// Declare one exception class per `GraphError` variant and register them.
///
/// The `.code` is set on the class object, so it reads the same off the class
/// (`mushroomdb.KeyNotFound.code`) and off a caught instance (`e.code`).
macro_rules! engine_errors {
    ($($class:ident, $code:literal, $doc:literal;)*) => {
        $(pyo3::create_exception!(mushroomdb, $class, MushroomError, $doc);)*

        fn register_errors(m: &Bound<'_, PyModule>) -> PyResult<()> {
            let py = m.py();
            let base = py.get_type::<MushroomError>();
            // The base is never raised, so it names no variant and no code.
            base.setattr("code", py.None())?;
            m.add("MushroomError", base)?;
            $(
                let ty = py.get_type::<$class>();
                ty.setattr("code", $code)?;
                m.add(stringify!($class), ty)?;
            )*
            Ok(())
        }
    };
}

engine_errors! {
    KeyNotFound, "key_not_found", "No node with this key. Carries `.key`.";
    DuplicateKey, "duplicate_key", "A node with this key already exists. Carries `.key`.";
    IoError, "io", "The store's filesystem refused a read or a write.";
    Corrupt, "corrupt", "The store's on-disk state did not parse. Carries `.detail`.";
    RuleInvalid, "rule_invalid", "The rule definition was rejected. Carries `.detail`.";
    RuleOwned, "rule_owned", "The edge belongs to a rule and is not writable by hand. Carries `.detail`.";
    RuleNotFound, "rule_not_found", "No rule by this name. Carries `.name`.";
    QueryError, "query_error", "The Cypher statement failed. Carries `.detail`, which is also the message.";
    IngestError, "ingest_error", "The batch was rejected before anything landed. Carries `.detail`.";
    ReadOnly, "read_only", "This handle never writes: an as-of instance, or one `scoped()` produced.";
    CommitOutOfRange, "commit_out_of_range", "The commit is outside the retained range `floor..total`. Carries `.commit`, `.total`, `.floor`.";
    ViewPropReadOnly, "view_prop_read_only", "The property is managed by a view. Carries `.view_name`.";
    CasConflict, "cas_conflict", "A compare-and-set precondition failed. Carries `.key`, `.expected`, `.actual`.";
    MaskedReadOnly, "masked_read_only", "A write statement reached a scoped or masked query path, which is read-only.";
    RoleWriteDenied, "role_write_denied", "A role-bound write was denied. Carries `.reason`, which is also the message.";
    MushroomBusy, "busy", "Another process holds the store's write lock.\n\nNothing was written, so retrying later is always safe. Raised only by write calls: opening read-only and reading never take the lock. Carries `.holder`, the holding process id when the platform makes it cheaply knowable and `None` otherwise — a diagnostic hint, never something to branch on.";
    NamespaceImmutable, "namespace_immutable", "A namespace is set at insert and fixed for the node's lifetime. Carries `.key`, `.from_` (spelled with a trailing underscore: `from` is a Python keyword) and `.to`.";
    CrossNamespace, "cross_namespace", "A hand-written edge would cross a namespace boundary. Carries `.src`, `.src_ns`, `.dst`, `.dst_ns`.";
    CommitTimeNotMonotonic, "commit_time_not_monotonic", "An asserted commit time would move the store's recorded history backwards. Carries `.supplied_ms` and `.newest_ms`. Raised by `record_commits_at` only — the live clock is never held to this, so an NTP step backwards still commits. Backfill in chronological order.";
    NoRecordedTime, "no_recorded_time", "A date was given to a history call on a store that records no commit times.\n\nWritten by a release before 0.6.11, or its `commit_times.bin` sidecar was removed. The store is intact and every commit-indexed read still works; only date resolution is unavailable. Pass a commit index instead. Deliberately distinct from `Corrupt`: an absent map is not a damaged one.";
    TimeBeforeFloor, "time_before_floor", "The date predates the oldest commit time the store still records. Carries `.floor_ms` and `.floor_commit`, the oldest entry the map can answer from. Retrying with an earlier instant fails the same way.";
}

#[pymodule]
fn mushroomdb(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<GraphDb>()?;
    register_errors(m)?;
    Ok(())
}
