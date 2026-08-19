use core_api::{
    Direction, Explanation, GraphDb as CoreDb, GraphError, NodeInfo, PredicateSummary, ResultSet,
    RuleDef, Value,
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

    fn insert_node(&self, key: &str, label: &str, props: Bound<'_, PyDict>) -> PyResult<()> {
        let mapped = dict_to_props(&props)?;
        self.with_mut(|db| db.insert_node(label, key, mapped))
    }

    fn insert_edge(&self, edge_type: &str, src: &str, dst: &str) -> PyResult<bool> {
        self.with_mut(|db| db.insert_edge(edge_type, src, dst))
    }

    fn set_prop(&self, key: &str, field: &str, value: Bound<'_, PyAny>) -> PyResult<()> {
        let v = py_to_value(&value)?;
        self.with_mut(|db| db.set_prop(key, field, v))
    }

    fn query(&self, py: Python<'_>, cypher: &str) -> PyResult<Vec<Py<PyDict>>> {
        let rs = self.with_ref(|db| db.query(cypher, &BTreeMap::new()))?;
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
    if obj.is_instance_of::<PyDict>() {
        return Err(PyTypeError::new_err(
            "dict is not a Value; nested maps are not supported",
        ));
    }
    Err(PyTypeError::new_err(format!(
        "cannot convert {} to Value (need str, int, float, bool, or list)",
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

fn rule_from_py(py: Python<'_>, rule: &Bound<'_, PyAny>) -> PyResult<RuleDef> {
    let json = py.import("json")?;
    let s: String = json.call_method1("dumps", (rule,))?.extract()?;
    serde_json::from_str(&s)
        .map_err(|e| PyValueError::new_err(format!("create_rule JSON does not match RuleDef: {e}")))
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
