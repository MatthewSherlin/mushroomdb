//! Shared JSON wire shapes for HTTP (`?format=json`) and MCP tool payloads.
//!
//! Cells are untagged JSON scalars (the inverse of [`core_api::json_to_value`]),
//! not `Value`'s internally-tagged serde form.

use core_api::{json_to_value, EdgeInfo, NodeInfo, ResultSet, Value};
use serde_json::{json, Value as Js};
use std::collections::BTreeMap;

pub(crate) fn value_to_json(v: &Value) -> Js {
    match v {
        Value::Int(i) => json!(i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Js::Number)
            .unwrap_or(Js::Null),
        Value::Str(s) => json!(s),
        Value::Bool(b) => json!(b),
        Value::List(xs) => Js::Array(xs.iter().map(value_to_json).collect()),
    }
}

pub(crate) fn node_info_json(info: &NodeInfo) -> Js {
    let props: serde_json::Map<String, Js> = info
        .props
        .iter()
        .map(|(k, v)| (k.clone(), value_to_json(v)))
        .collect();
    json!({
        "key": info.key,
        "label": info.label,
        "props": props,
    })
}

pub(crate) fn node_edges_json(edges: &[EdgeInfo]) -> Js {
    json!({
        "edges": edges.iter().map(|e| json!({
            "edge_type": e.edge_type,
            "src_key": e.src_key,
            "dst_key": e.dst_key,
            "derived": e.derived,
        })).collect::<Vec<_>>()
    })
}

pub(crate) fn result_set_json(rs: &ResultSet) -> Js {
    let columns: Vec<&str> = rs.columns().iter().map(String::as_str).collect();
    let rows: Vec<Vec<Js>> = (0..rs.len())
        .map(|i| {
            rs.row(i)
                .iter()
                .map(|cell| cell.as_ref().map(value_to_json).unwrap_or(Js::Null))
                .collect()
        })
        .collect();
    json!({ "columns": columns, "rows": rows })
}

/// Map JSON object values through [`json_to_value`]. Only JSON scalars
/// (bool / number / string) are accepted as query params.
pub(crate) fn params_from_json(v: Option<&Js>) -> Result<BTreeMap<String, Value>, String> {
    let Some(v) = v else {
        return Ok(BTreeMap::new());
    };
    let obj = v
        .as_object()
        .ok_or_else(|| "params must be an object".to_string())?;
    let mut out = BTreeMap::new();
    for (k, val) in obj {
        if !matches!(val, Js::Bool(_) | Js::Number(_) | Js::String(_)) {
            return Err(format!("param {k} is not a JSON scalar"));
        }
        match json_to_value(val.clone()) {
            Some(mapped) => {
                out.insert(k.clone(), mapped);
            }
            None => return Err(format!("param {k} is not a JSON scalar")),
        }
    }
    Ok(out)
}

/// User edges for POST /ingest and MCP `ingest_json`.
///
/// Shape: `[{edge_type, src, dst}]`. Missing fields are a 400 string;
/// unknown endpoints fail later via `insert_edge` (`KeyNotFound` → 400).
pub(crate) fn parse_ingest_edges(v: &Js) -> Result<Vec<(String, String, String)>, String> {
    let arr = v
        .as_array()
        .ok_or_else(|| "edges must be an array".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("edges[{i}] must be an object"))?;
        let edge_type = obj
            .get("edge_type")
            .and_then(Js::as_str)
            .ok_or_else(|| format!("edges[{i}] missing edge_type"))?;
        let src = obj
            .get("src")
            .and_then(Js::as_str)
            .ok_or_else(|| format!("edges[{i}] missing src"))?;
        let dst = obj
            .get("dst")
            .and_then(Js::as_str)
            .ok_or_else(|| format!("edges[{i}] missing dst"))?;
        out.push((edge_type.to_string(), src.to_string(), dst.to_string()));
    }
    Ok(out)
}
