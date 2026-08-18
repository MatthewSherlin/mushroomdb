//! Shared JSON wire shapes for HTTP (`?format=json`) and MCP tool payloads.
//!
//! Cells are untagged JSON scalars (the inverse of [`core_api::json_to_value`]),
//! not `Value`'s internally-tagged serde form.

use core_api::{json_to_value, ResultSet, Value};
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
