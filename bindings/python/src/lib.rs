use core_api::{
    default_max_edges, Direction, Explanation, GraphDb as CoreDb, GraphError, NodeInfo,
    PredicateSummary, ResultSet, RuleDef, Value,
};
use core_storage::fs::RealFs;
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

type Db = CoreDb<RealFs>;

struct Inner(Mutex<Option<Db>>);

#[pyclass(name = "GraphDb")]
struct GraphDb {
    inner: Inner,
}

#[pymethods]
impl GraphDb {
    #[staticmethod]
    fn open(path: PathBuf) -> PyResult<Self> {
        let db = CoreDb::open(&path).map_err(graph_err)?;
        Ok(GraphDb {
            inner: Inner(Mutex::new(Some(db))),
        })
    }

    fn insert_node(&self, label: &str, key: &str, props: Bound<'_, PyDict>) -> PyResult<()> {
        let mapped = dict_to_props(&props)?;
        self.with_mut(|db| db.insert_node(label, key, mapped))
    }

    fn insert_edge(&self, edge_type: &str, src: &str, dst: &str) -> PyResult<bool> {
        self.with_mut(|db| db.insert_edge(edge_type, src, dst))
    }

    /// Delete a user-owned edge.  Returns `True` if the edge existed and was
    /// removed, `False` if it was not present.  Raises `RuntimeError` if the
    /// edge is rule-derived (must retract by changing properties instead).
    fn delete_edge(&self, edge_type: &str, src: &str, dst: &str) -> PyResult<bool> {
        self.with_mut(|db| db.delete_edge(edge_type, src, dst))
    }

    fn set_prop(&self, key: &str, field: &str, value: Bound<'_, PyAny>) -> PyResult<()> {
        let v = py_to_value(&value)?;
        self.with_mut(|db| db.set_prop(key, field, v))
    }

    /// Execute a read query, optionally with named parameters.
    ///
    /// `params` may be:
    /// - omitted (no parameters)
    /// - a `dict` mapping name→value (ergonomic form)
    /// - a list of `(name, value)` tuples (back-compat with `query_with_params`)
    ///
    /// Values must be `int`, `float`, `str`, `bool`, `list`, or `dict`.
    #[pyo3(signature = (cypher, params = None))]
    fn query(
        &self,
        py: Python<'_>,
        cypher: &str,
        params: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Vec<Py<PyDict>>> {
        let map = params_to_map(params)?;
        let rs = self.with_ref(|db| db.query(cypher, &map))?;
        result_set_to_rows(py, &rs)
    }

    /// Execute a read query with named parameters (back-compat alias for
    /// `query(cypher, params=[...])` with a tuple-list).
    ///
    /// ```python
    /// rows = db.query_with_params(
    ///     "MATCH (n:Person) WHERE n.age > $min RETURN n.key",
    ///     [("min", 18)],
    /// )
    /// ```
    ///
    /// Each element of `params` is a `(name, value)` tuple.  Values must be
    /// `int`, `float`, `str`, `bool`, or a `list` of those.
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
        let rs = self.with_ref(|db| db.query(cypher, &map))?;
        result_set_to_rows(py, &rs)
    }

    /// Rename a node's key.  The dense id (edges, history, last-change) is
    /// unchanged.
    ///
    /// Raises `RuntimeError` with `KeyNotFound` if `old` is unknown, or
    /// `DuplicateKey` if `new` is already live.
    fn rename_node(&self, old: &str, new: &str) -> PyResult<()> {
        self.with_mut(|db| db.rename_node(old, new))
    }

    /// Insert an edge, auto-creating any missing endpoint.
    ///
    /// Each missing endpoint is created as a plain node with label
    /// `placeholder_label` and no properties.  Rules fire and last-change is
    /// updated for each auto-created node.  Returns a dict with keys
    /// `nodes_created` and `edge_inserted`.
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
    /// and `deleted`.
    ///
    /// Params follow the same `[(name, value)]` convention as
    /// `query_with_params`.
    #[pyo3(signature = (cypher, params = None))]
    fn query_write(
        &self,
        py: Python<'_>,
        cypher: &str,
        params: Option<Bound<'_, PyList>>,
    ) -> PyResult<Vec<Py<PyDict>>> {
        let mut map = BTreeMap::new();
        if let Some(pl) = params {
            for item in pl.iter() {
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
        }
        let rs = self.with_mut(|db| db.query_write(cypher, &map))?;
        result_set_to_rows(py, &rs)
    }

    fn create_rule(&self, py: Python<'_>, rule: Bound<'_, PyAny>) -> PyResult<()> {
        let def = rule_from_py(py, &rule)?;
        self.with_mut(|db| db.create_rule(def))
    }

    fn explain(&self, py: Python<'_>, a: &str, b: &str) -> PyResult<Vec<Py<PyDict>>> {
        let rows = self.with_ref(|db| db.explain(a, b))?;
        rows.iter().map(|e| explanation_to_py(py, e)).collect()
    }

    fn neighbors(&self, key: &str, edge_type: &str, direction: &str) -> PyResult<Vec<String>> {
        let dir = parse_dir(direction)?;
        self.with_ref(|db| db.neighbors(key, edge_type, dir))
    }

    /// Unknown key: `None`, matching Rust `GraphDb::node_info` → `Option`.
    /// Contrast `node_edges`, which raises `RuntimeError` for the same miss
    /// because Rust returns `Result` (`GraphError::KeyNotFound`). Deliberate.
    fn node_info(&self, py: Python<'_>, key: &str) -> PyResult<Option<Py<PyDict>>> {
        let info = self.with_ref(|db| Ok(db.node_info(key)))?;
        match info {
            Some(info) => Ok(Some(node_info_to_py(py, &info)?)),
            None => Ok(None),
        }
    }

    /// Unknown key: `RuntimeError` (`node key not found: …`), matching Rust
    /// `GraphDb::node_edges` → `Result`. `node_info` stays `None` on the same
    /// miss (`Option`). The asymmetry is the core API, not a Python invention.
    fn node_edges(&self, py: Python<'_>, key: &str) -> PyResult<Vec<Py<PyDict>>> {
        let edges = self.with_ref(|db| db.node_edges(key))?;
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

    /// Atomically ingest `nodes` (each `{key, label, props}`) and optional
    /// `edges` (each `{edge_type, src, dst}`) in a single WAL commit.
    ///
    /// A bad edge (unknown endpoint, rule-owned, …) rejects the **entire**
    /// batch — nothing is committed and `RuntimeError` is raised.
    ///
    /// Returns a dict matching `IngestReport` shape:
    /// `{inserted, edges_inserted, row_errors, rules_created, skipped_fk_fields}`.
    /// `edges_inserted` counts only newly written edges; duplicate edges that
    /// already exist are silent no-ops and are NOT counted.
    ///
    /// **Performance note**: for large datasets keep each call to ≤10 000 nodes.
    /// A single call with 100 000+ nodes serialises one giant WAL frame whose
    /// fsync cost dominates and negates the batching benefit.  Chunk at the
    /// call site (e.g. `for chunk in batched(nodes, 10_000)`).
    #[pyo3(signature = (nodes, edges=None))]
    fn ingest_batch(
        &self,
        py: Python<'_>,
        nodes: Bound<'_, PyList>,
        edges: Option<Bound<'_, PyList>>,
    ) -> PyResult<Py<PyDict>> {
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
            let props_dict = props_obj.downcast::<PyDict>().map_err(|_| {
                PyTypeError::new_err("node 'props' must be a dict")
            })?;
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
        let (nodes_inserted, edges_inserted) = self.with_mut(|db| {
            let mut batch = db.batch();
            for (label, key, props) in node_ops {
                batch.insert_node(&label, &key, props);
            }
            for (edge_type, src, dst) in &edge_ops {
                batch.insert_edge(edge_type, src, dst);
            }
            batch.commit()
        })?;

        // Build IngestReport-shaped dict with accurate counts.
        let d = PyDict::new(py);
        d.set_item("inserted", nodes_inserted)?;
        d.set_item("edges_inserted", edges_inserted)?;
        d.set_item("row_errors", PyList::empty(py))?;
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
    #[pyo3(signature = (inserts=None, deletes=None))]
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
        let insert_ops = inserts.as_ref().map(parse_edges).transpose()?.unwrap_or_default();
        let delete_ops = deletes.as_ref().map(parse_edges).transpose()?.unwrap_or_default();
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

    /// Return database statistics: node/edge counts plus per-rule provenance
    /// size, trip latch, and fire counter.  Shape matches the HTTP `/stats`
    /// JSON response.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        let s = self.with_ref(|db| Ok(db.stats()))?;
        let d = PyDict::new(py);
        d.set_item("nodes_live", s.nodes_live)?;
        d.set_item("nodes_tombstoned", s.nodes_tombstoned)?;
        d.set_item("edges", s.edges)?;
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
        Ok(d.unbind())
    }

    /// Write a durable snapshot and truncate the WAL tail.
    ///
    /// After `snapshot()`, the next `GraphDb.open()` on the same path loads
    /// the snapshot directly and skips WAL replay, making reopen significantly
    /// faster for large databases.
    fn snapshot(&self) -> PyResult<()> {
        self.with_mut(|db| db.snapshot())
    }

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
    fn with_mut<T, F>(&self, f: F) -> PyResult<T>
    where
        F: FnOnce(&mut Db) -> core_api::Result<T>,
    {
        let mut guard = lock(&self.inner.0)?;
        let db = guard
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("GraphDb is closed"))?;
        f(db).map_err(graph_err)
    }

    fn with_ref<T, F>(&self, f: F) -> PyResult<T>
    where
        F: FnOnce(&Db) -> core_api::Result<T>,
    {
        let guard = lock(&self.inner.0)?;
        let db = guard
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("GraphDb is closed"))?;
        f(db).map_err(graph_err)
    }
}

fn lock<T>(m: &Mutex<T>) -> PyResult<std::sync::MutexGuard<'_, T>> {
    m.lock()
        .map_err(|_| PyRuntimeError::new_err("GraphDb lock poisoned"))
}

fn graph_err(e: GraphError) -> PyErr {
    match e {
        GraphError::QueryError { detail } => PyRuntimeError::new_err(detail),
        other => PyRuntimeError::new_err(other.to_string()),
    }
}

fn parse_dir(s: &str) -> PyResult<Direction> {
    match s.to_ascii_lowercase().as_str() {
        "out" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        _ => Err(PyValueError::new_err("direction must be 'out' or 'in'")),
    }
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

fn dict_to_props(props: &Bound<'_, PyDict>) -> PyResult<Vec<(String, Value)>> {
    let mut out = Vec::with_capacity(props.len());
    for (k, v) in props.iter() {
        let key: String = k.extract()?;
        out.push((key, py_to_value(&v)?));
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
    let mut def: RuleDef = serde_json::from_str(&s).map_err(|e| {
        PyValueError::new_err(format!("create_rule JSON does not match RuleDef: {e}"))
    })?;
    if missing_max_edges {
        def.max_edges = Some(default_max_edges(&def.predicate));
    }
    Ok(def)
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

#[pymodule]
fn mushroomdb(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<GraphDb>()?;
    Ok(())
}
