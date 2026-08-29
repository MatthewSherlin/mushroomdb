//! Shared JSON wire shapes for HTTP (`?format=json`) and MCP tool payloads.
//!
//! Cells are untagged JSON scalars (the inverse of [`core_api::json_to_value`]),
//! not `Value`'s internally-tagged serde form.

use core_api::{
    default_max_edges, json_to_value, EdgeEvent, EdgeHistoryEvent, EdgeInfo, HistoryChange,
    HistoryEntry, HistoryResult, MaskedEdge, NodeInfo, ResultSet, RuleDef, Value,
};
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
        Value::Map(m) => {
            let obj: serde_json::Map<String, Js> = m
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            Js::Object(obj)
        }
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

/// Stub JSON for a node that exists but is hidden from a mask.
/// Shape: `{"key": "<key>", "restricted": true}` — no other fields.
pub(crate) fn stub_node_json(key: &str) -> Js {
    json!({"key": key, "restricted": true})
}

/// Render a [`MaskedEdge`] list to the `node_edges` wire shape.
///
/// Hidden endpoints are rendered as `{"key": "<key>", "restricted": true}`;
/// visible endpoints are rendered as plain key strings.
pub(crate) fn masked_edges_json(edges: &[MaskedEdge]) -> Js {
    let items: Vec<Js> = edges
        .iter()
        .map(|e| {
            let src = if e.src_restricted {
                json!({"key": e.src_key, "restricted": true})
            } else {
                json!(e.src_key)
            };
            let dst = if e.dst_restricted {
                json!({"key": e.dst_key, "restricted": true})
            } else {
                json!(e.dst_key)
            };
            json!({
                "edge_type": e.edge_type,
                "src_key": src,
                "dst_key": dst,
                "derived": e.derived,
            })
        })
        .collect();
    json!({"edges": items})
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

/// Map JSON object values through [`json_to_value`]. Scalars, arrays, and
/// objects are all accepted; `null` params are silently dropped.
pub(crate) fn params_from_json(v: Option<&Js>) -> Result<BTreeMap<String, Value>, String> {
    let Some(v) = v else {
        return Ok(BTreeMap::new());
    };
    let obj = v
        .as_object()
        .ok_or_else(|| "params must be an object".to_string())?;
    let mut out = BTreeMap::new();
    for (k, val) in obj {
        match json_to_value(val.clone()) {
            Some(mapped) => {
                out.insert(k.clone(), mapped);
            }
            None => {
                // null params are silently omitted
            }
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

/// Serialize a [`HistoryChange`] variant to an untagged JSON object.
///
/// The `"type"` field names the variant; payload fields follow inline.
pub(crate) fn history_change_json(change: &HistoryChange) -> Js {
    match change {
        HistoryChange::NodeInserted { label } => json!({"type": "NodeInserted", "label": label}),
        HistoryChange::PropSet { field, value } => json!({
            "type": "PropSet",
            "field": field,
            "value": value_to_json(value),
        }),
        HistoryChange::PropRemoved { field } => json!({"type": "PropRemoved", "field": field}),
        HistoryChange::EdgeAdded {
            edge_type,
            other,
            outgoing,
        } => json!({
            "type": "EdgeAdded",
            "edge_type": edge_type,
            "other": other,
            "outgoing": outgoing,
        }),
        HistoryChange::EdgeRemoved {
            edge_type,
            other,
            outgoing,
        } => json!({
            "type": "EdgeRemoved",
            "edge_type": edge_type,
            "other": other,
            "outgoing": outgoing,
        }),
        HistoryChange::NodeDeleted => json!({"type": "NodeDeleted"}),
    }
}

pub(crate) fn history_entry_json(entry: &HistoryEntry) -> Js {
    json!({
        "commit": entry.commit,
        "change": history_change_json(&entry.change),
    })
}

pub(crate) fn edge_history_result_json(
    a: &str,
    b: &str,
    result: &HistoryResult<EdgeHistoryEvent>,
) -> Js {
    let events: Vec<Js> = result
        .items
        .iter()
        .map(|ev| {
            let event_str = match ev.event {
                EdgeEvent::Added => "Added",
                EdgeEvent::Retracted => "Retracted",
            };
            json!({
                "edge_type": ev.edge_type,
                "commit": ev.commit,
                "event": event_str,
                "rule": ev.rule,
            })
        })
        .collect();
    json!({
        "a": a,
        "b": b,
        "events": events,
        "total_commits": result.total_commits,
    })
}

pub(crate) fn node_history_json(key: &str, entries: &[HistoryEntry], total_commits: u64) -> Js {
    let history: Vec<Js> = entries.iter().map(history_entry_json).collect();
    json!({
        "key": key,
        "history": history,
        "total_commits": total_commits,
    })
}

/// Deserialize a `RuleDef` from HTTP/MCP JSON. Omitted or null `max_edges`
/// fills `default_max_edges`. Do not add `#[serde(default)]` on
/// `RuleDef.max_edges` — bincode is positional.
pub(crate) fn rule_def_from_json(v: Js) -> Result<RuleDef, String> {
    let mut def: RuleDef = serde_json::from_value(v).map_err(|e| e.to_string())?;
    if def.max_edges.is_none() {
        def.max_edges = Some(default_max_edges(&def.predicate));
    }
    Ok(def)
}
