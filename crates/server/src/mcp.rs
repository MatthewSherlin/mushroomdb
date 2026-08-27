//! MCP server: JSON-RPC 2.0 over newline-delimited stdio.
//!
//! Framing is **one JSON object per line**. LSP-style `Content-Length`
//! headers are not accepted — a header line is a parse error (`-32700`)
//! and the loop continues. Blank lines are skipped.
//!
//! # Methods
//!
//! - `initialize` — `protocolVersion` `"2024-11-05"`, `capabilities.tools`,
//!   `serverInfo.name` `"mushroomdb"`
//! - `notifications/initialized` — ignored
//! - `tools/list` — the eleven tools below, each with a JSON Schema
//! - `tools/call` — dispatch; success is
//!   `{content:[{type:"text", text:<json string>}]}`
//!
//! Unknown methods on a **request** (has `id`) → `-32601`. A notification
//! (no `id` member) never writes a response, including unknown methods.
//!
//! # Error split
//!
//! Protocol errors are JSON-RPC `error` objects:
//! - `-32700` parse — unparseable line (invalid JSON / invalid UTF-8)
//! - `-32600` invalid request — parsed JSON that is not an object, or a
//!   request with missing / non-string `method`
//! - `-32601` method — unknown `method` on a request
//! - `-32602` params — `tools/call` envelope invalid: `params` not an object,
//!   missing / non-string `name`, `arguments` present but not an object,
//!   or unknown tool name
//!
//! Tool-level failures are JSON-RPC **results** with `isError: true` and a
//! text message: missing or wrong-typed fields inside a known tool's
//! `arguments`, and every [`GraphError`] from core-api.
//!
//! # Deadlock
//!
//! [`SharedDb::read`] / [`SharedDb::write`] guards are held only for the
//! public core-api call, then dropped before serializing or writing. Do not
//! nest a second lock on the same handle (the `RwLock` is not re-entrant).
//!
//! EOF on `reader` returns `Ok(())`. Read/write I/O errors propagate.

use crate::json::{
    node_edges_json, node_info_json, params_from_json, parse_ingest_edges, result_set_json,
    rule_def_from_json,
};
use core_api::{
    json_to_rows, json_to_value, AutoFk, Dir, GraphError, IngestOptions, NodeMask, SharedDb, Value,
};
use serde_json::{json, Value as Js};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

/// Run the MCP loop until `reader` hits EOF.
pub fn run_mcp_stdio(
    db: SharedDb,
    mut reader: impl BufRead,
    mut writer: impl Write,
) -> io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        match std::str::from_utf8(&buf) {
            Ok(s) if s.trim().is_empty() => continue,
            Ok(s) => handle_line(&db, s.trim(), &mut writer)?,
            Err(_) => write_error(&mut writer, None, -32700, "Parse error")?,
        }
    }
}

fn handle_line(db: &SharedDb, line: &str, writer: &mut impl Write) -> io::Result<()> {
    let msg: Js = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return write_error(writer, None, -32700, "Parse error"),
    };
    let Some(obj) = msg.as_object() else {
        return write_error(writer, None, -32600, "Invalid Request");
    };
    let is_request = obj.contains_key("id");
    let id = obj.get("id").cloned();
    let method = match obj.get("method").and_then(Js::as_str) {
        Some(m) => m,
        None => {
            if is_request {
                write_error(writer, id, -32600, "Invalid Request")?;
            }
            return Ok(());
        }
    };
    match method {
        "initialize" => {
            if is_request {
                write_result(writer, id, initialize_result())?;
            }
        }
        "notifications/initialized" => {
            if is_request {
                write_result(writer, id, json!({}))?;
            }
        }
        "tools/list" => {
            if is_request {
                write_result(writer, id, tools_list())?;
            }
        }
        "tools/call" => {
            if is_request {
                match dispatch_call(db, obj.get("params")) {
                    CallOutcome::Protocol { code, message } => {
                        write_error(writer, id, code, &message)?;
                    }
                    CallOutcome::ToolOk(payload) => {
                        write_result(writer, id, tool_ok(payload))?;
                    }
                    CallOutcome::ToolErr(message) => {
                        write_result(writer, id, tool_err(&message))?;
                    }
                }
            }
        }
        _ => {
            if is_request {
                write_error(writer, id, -32601, "Method not found")?;
            }
        }
    }
    Ok(())
}

enum CallOutcome {
    Protocol { code: i64, message: String },
    ToolOk(Js),
    ToolErr(String),
}

fn dispatch_call(db: &SharedDb, params: Option<&Js>) -> CallOutcome {
    let Some(params) = params.and_then(Js::as_object) else {
        return protocol_invalid();
    };
    let Some(name) = params.get("name").and_then(Js::as_str) else {
        return protocol_invalid();
    };
    let empty = json!({});
    let args = match params.get("arguments") {
        None => &empty,
        Some(a) if a.is_object() => a,
        Some(_) => return protocol_invalid(),
    };
    match name {
        "query" => tool_query(db, args),
        "ingest_json" => tool_ingest(db, args),
        "create_rule" => tool_create_rule(db, args),
        "explain" => tool_explain(db, args),
        "stats" => tool_stats(db),
        "neighborhood" => tool_neighborhood(db, args),
        "node_info" => tool_node_info(db, args),
        "node_edges" => tool_node_edges(db, args),
        "upsert_entity" => tool_upsert_entity(db, args),
        "find_similar" => tool_find_similar(db, args),
        "explain_association" => tool_explain(db, args),
        _ => protocol_invalid(),
    }
}

fn protocol_invalid() -> CallOutcome {
    CallOutcome::Protocol {
        code: -32602,
        message: "Invalid params".into(),
    }
}

fn tool_query(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(cypher) = args.get("cypher").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing cypher".into());
    };
    let params = match params_from_json(args.get("params")) {
        Ok(p) => p,
        Err(e) => return CallOutcome::ToolErr(e),
    };

    // Optional mask: when present, route to query_masked (read-only).
    if let Some(mask_val) = args.get("mask") {
        let keys = match mask_val.as_array() {
            Some(arr) => {
                let mut ks: Vec<String> = Vec::with_capacity(arr.len());
                for v in arr {
                    match v.as_str() {
                        Some(s) => ks.push(s.to_string()),
                        None => {
                            return CallOutcome::ToolErr("mask must be an array of strings".into())
                        }
                    }
                }
                ks
            }
            None => return CallOutcome::ToolErr("mask must be an array of strings".into()),
        };
        let g = db.read();
        let mask = NodeMask::from_keys(&*g, keys.iter().map(String::as_str));
        return match g.query_masked(cypher, &params, &mask) {
            Ok(rs) => CallOutcome::ToolOk(result_set_json(&rs)),
            Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
        };
    }

    let is_write = match core_api::is_write_query(cypher) {
        Ok(b) => b,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let rs = if is_write {
        let mut g = db.write();
        g.query_write(cypher, &params)
    } else {
        let g = db.read();
        g.query(cypher, &params)
    };
    match rs {
        Ok(rs) => CallOutcome::ToolOk(result_set_json(&rs)),
        Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
    }
}

fn tool_ingest(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(label) = args.get("label").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing label".into());
    };
    let Some(rows_json) = args.get("rows_json").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing rows_json".into());
    };
    let mut opts = IngestOptions::default();
    if let Some(kf) = args.get("key_field") {
        match kf.as_str() {
            Some(s) => opts.key_field = s.to_string(),
            None => return CallOutcome::ToolErr("key_field must be a string".into()),
        }
    }
    if let Some(suf) = args.get("auto_fk_suffix") {
        match suf.as_str() {
            Some(s) => {
                opts.auto_fk = AutoFk::Auto {
                    suffix: s.to_string(),
                }
            }
            None => return CallOutcome::ToolErr("auto_fk_suffix must be a string".into()),
        }
    }
    let edges = match args.get("edges") {
        None | Some(Js::Null) => Vec::new(),
        Some(raw) => match parse_ingest_edges(raw) {
            Ok(e) => e,
            Err(e) => return CallOutcome::ToolErr(e),
        },
    };
    let parsed: Js = match serde_json::from_str(rows_json) {
        Ok(v) => v,
        Err(e) => {
            return CallOutcome::ToolErr(graph_err_msg(GraphError::IngestError {
                detail: e.to_string(),
            }))
        }
    };
    let mut converted = match json_to_rows(&parsed) {
        Ok(c) => c,
        Err(e) => return CallOutcome::ToolErr(graph_err_msg(e)),
    };
    let taken = std::mem::take(&mut converted.rows);
    let report = {
        let mut g = db.write();
        g.ingest_with_edges(label, taken, &opts, &edges)
    };
    match report.map(|r| converted.into_report(r)) {
        Ok(r) => match serde_json::to_value(&r) {
            Ok(v) => CallOutcome::ToolOk(v),
            Err(e) => CallOutcome::ToolErr(e.to_string()),
        },
        Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
    }
}

fn tool_create_rule(db: &SharedDb, args: &Js) -> CallOutcome {
    let def = match rule_def_from_json(args.clone()) {
        Ok(d) => d,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let name = def.name.clone();
    let res = {
        let mut g = db.write();
        g.create_rule(def)
    };
    match res {
        Ok(()) => CallOutcome::ToolOk(json!({"ok": true, "name": name})),
        Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
    }
}

fn tool_explain(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(a) = args.get("a").and_then(Js::as_str).filter(|s| !s.is_empty()) else {
        return CallOutcome::ToolErr("missing a".into());
    };
    let Some(b) = args.get("b").and_then(Js::as_str).filter(|s| !s.is_empty()) else {
        return CallOutcome::ToolErr("missing b".into());
    };
    let out = {
        let g = db.read();
        g.explain(a, b)
    };
    match out {
        Ok(v) => match serde_json::to_value(&v) {
            Ok(j) => CallOutcome::ToolOk(j),
            Err(e) => CallOutcome::ToolErr(e.to_string()),
        },
        Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
    }
}

fn tool_stats(db: &SharedDb) -> CallOutcome {
    let snap = {
        let g = db.read();
        g.stats()
    };
    match serde_json::to_value(&snap) {
        Ok(v) => CallOutcome::ToolOk(v),
        Err(e) => CallOutcome::ToolErr(e.to_string()),
    }
}

fn tool_neighborhood(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(key) = args.get("key").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing key".into());
    };
    let depth = match args.get("depth") {
        None => 1u32,
        Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(d) => d,
            None => return CallOutcome::ToolErr("depth must be an integer".into()),
        },
    };
    let dir = match args.get("direction") {
        None => Dir::Both,
        Some(v) => match v.as_str() {
            Some(s) if s.eq_ignore_ascii_case("out") => Dir::Out,
            Some(s) if s.eq_ignore_ascii_case("in") => Dir::In,
            Some(s) if s.eq_ignore_ascii_case("both") => Dir::Both,
            Some(other) => return CallOutcome::ToolErr(format!("unknown direction: {other}")),
            None => return CallOutcome::ToolErr("direction must be a string".into()),
        },
    };
    let edge_type_names: Option<Vec<String>> = match args.get("edge_types") {
        None => None,
        Some(v) => {
            let Some(arr) = v.as_array() else {
                return CallOutcome::ToolErr("edge_types must be an array of strings".into());
            };
            let mut names = Vec::with_capacity(arr.len());
            for item in arr {
                match item.as_str() {
                    Some(s) => names.push(s.to_string()),
                    None => {
                        return CallOutcome::ToolErr(
                            "edge_types must be an array of strings".into(),
                        )
                    }
                }
            }
            Some(names)
        }
    };
    let etype_refs: Option<Vec<&str>> = edge_type_names
        .as_ref()
        .map(|v| v.iter().map(String::as_str).collect());
    let rs = {
        let g = db.read();
        match g.node_ref(key) {
            Some(n) => Ok(n.neighborhood(depth, etype_refs.as_deref(), dir)),
            None => Err(GraphError::KeyNotFound {
                key: key.to_string(),
            }),
        }
    };
    match rs {
        Ok(rs) => CallOutcome::ToolOk(result_set_json(&rs)),
        Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
    }
}

fn tool_node_info(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(key) = args.get("key").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing key".into());
    };
    let info = {
        let g = db.read();
        g.node_info(key)
    };
    match info {
        Some(info) => CallOutcome::ToolOk(node_info_json(&info)),
        None => CallOutcome::ToolErr(graph_err_msg(GraphError::KeyNotFound {
            key: key.to_string(),
        })),
    }
}

fn tool_node_edges(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(key) = args.get("key").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing key".into());
    };
    let out = {
        let g = db.read();
        g.node_edges(key)
    };
    match out {
        Ok(edges) => CallOutcome::ToolOk(node_edges_json(&edges)),
        Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
    }
}

/// Insert a new node or update an existing node's properties, keyed by `key`.
///
/// If the node exists: each prop in `props` is written via `set_prop`.
/// If the node does not exist: `label` is required; the node is ingested with
/// `key_field = "id"` and the supplied props.
///
/// Returns `{ok, key, created, updated_fields?}`.
fn tool_upsert_entity(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(key) = args.get("key").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing key".into());
    };
    let label_opt = args.get("label").and_then(Js::as_str);
    let Some(props_obj) = args.get("props").and_then(Js::as_object) else {
        return CallOutcome::ToolErr("missing props".into());
    };

    let exists = {
        let g = db.read();
        g.has_node(key)
    };

    if exists {
        let mut g = db.write();
        let mut count = 0usize;
        for (field, json_val) in props_obj {
            match json_to_value(json_val.clone()) {
                Some(v) => {
                    if let Err(e) = g.set_prop(key, field, v) {
                        return CallOutcome::ToolErr(graph_err_msg(e));
                    }
                    count += 1;
                }
                None => {
                    return CallOutcome::ToolErr(format!(
                        "prop {field} is not a supported value type"
                    ))
                }
            }
        }
        CallOutcome::ToolOk(json!({
            "ok": true,
            "key": key,
            "created": false,
            "updated_fields": count
        }))
    } else {
        let Some(label) = label_opt else {
            return CallOutcome::ToolErr("label required when creating a new entity".into());
        };
        let mut row: BTreeMap<String, Value> = BTreeMap::new();
        row.insert("id".to_string(), Value::Str(key.to_string()));
        for (field, json_val) in props_obj {
            if field == "id" {
                continue;
            }
            match json_to_value(json_val.clone()) {
                Some(v) => {
                    row.insert(field.clone(), v);
                }
                None => {
                    return CallOutcome::ToolErr(format!(
                        "prop {field} is not a supported value type"
                    ))
                }
            }
        }
        let opts = IngestOptions {
            key_field: "id".to_string(),
            auto_fk: AutoFk::Off,
        };
        let mut g = db.write();
        match g.ingest(label, vec![row], &opts) {
            Ok(_) => CallOutcome::ToolOk(json!({ "ok": true, "key": key, "created": true })),
            Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
        }
    }
}

/// Return neighbors connected by a given edge type (default `"SIMILAR"`).
///
/// Results are read from edges already materialized by a derivation rule
/// (e.g. a `VectorSimilar` rule). Without a matching rule the returned list
/// is empty — no live cosine computation is performed here.
/// Returns up to `limit` (default 10) neighbor entries.
fn tool_find_similar(db: &SharedDb, args: &Js) -> CallOutcome {
    // When a `vector` array is provided, use the HNSW / brute-force vector
    // similarity path instead of looking up pre-derived edges.
    if let Some(vec_js) = args.get("vector").and_then(Js::as_array) {
        let q: Vec<f64> = vec_js.iter().filter_map(|v| v.as_f64()).collect();
        if q.is_empty() {
            return CallOutcome::ToolErr("vector must be a non-empty array of numbers".into());
        }
        let field = args
            .get("field")
            .and_then(Js::as_str)
            .unwrap_or("embedding");
        let label = args.get("label").and_then(Js::as_str).unwrap_or("");
        let k = args
            .get("k")
            .and_then(Js::as_u64)
            .map(|n| n as usize)
            .unwrap_or(10);
        let min = args.get("min").and_then(Js::as_f64).unwrap_or(0.8);

        let hits = {
            let g = db.read();
            g.find_similar_vector(field, label, &q, k, min)
        };
        let results: Vec<Js> = hits
            .into_iter()
            .map(|(key, score)| json!({ "key": key, "score": score }))
            .collect();
        return CallOutcome::ToolOk(json!({
            "mode": "vector",
            "field": field,
            "label": label,
            "k": k,
            "min": min,
            "results": results
        }));
    }

    // Edge-traversal path: return neighbors connected by the given edge type.
    let Some(key) = args.get("key").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing key (or provide vector for vector search)".into());
    };
    let edge_type = args
        .get("edge_type")
        .and_then(Js::as_str)
        .unwrap_or("SIMILAR");
    let limit = args
        .get("limit")
        .and_then(Js::as_u64)
        .map(|n| n as usize)
        .unwrap_or(10);

    let out = {
        let g = db.read();
        g.node_edges(key)
    };
    match out {
        Ok(edges) => {
            let similar: Vec<Js> = edges
                .iter()
                .filter(|e| e.edge_type == edge_type)
                .take(limit)
                .map(|e| {
                    let neighbor_key = if e.src_key == key {
                        &e.dst_key
                    } else {
                        &e.src_key
                    };
                    let direction = if e.src_key == key { "out" } else { "in" };
                    json!({
                        "neighbor_key": neighbor_key,
                        "direction": direction,
                        "edge_type": e.edge_type,
                        "derived": e.derived,
                    })
                })
                .collect();
            CallOutcome::ToolOk(json!({
                "key": key,
                "edge_type": edge_type,
                "similar": similar
            }))
        }
        Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
    }
}

fn graph_err_msg(e: GraphError) -> String {
    match e {
        GraphError::QueryError { detail } | GraphError::IngestError { detail } => detail,
        other => other.to_string(),
    }
}

fn initialize_result() -> Js {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "mushroomdb" }
    })
}

fn tools_list() -> Js {
    json!({
        "tools": [
            {
                "name": "query",
                "description": "Run a Cypher query (read or write) against the graph. When 'mask' is provided, only the listed node keys are visible (read-only).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cypher": { "type": "string", "description": "Cypher query text." },
                        "params": {
                            "type": "object",
                            "description": "Named JSON-scalar query parameters."
                        },
                        "mask": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional node key allow-list. When present, only these nodes are visible; write statements are rejected."
                        }
                    },
                    "required": ["cypher"]
                }
            },
            {
                "name": "ingest_json",
                "description": "Ingest a JSON array of objects as nodes of one label.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string" },
                        "rows_json": {
                            "type": "string",
                            "description": "JSON text of an array of objects."
                        },
                        "key_field": { "type": "string" },
                        "auto_fk_suffix": { "type": "string" },
                        "edges": {
                            "type": "array",
                            "description": "Optional user edges [{edge_type, src, dst}]."
                        }
                    },
                    "required": ["label", "rows_json"]
                }
            },
            {
                "name": "create_rule",
                "description": "Create a derivation rule (RuleDef JSON).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "src_label": { "type": "string" },
                        "dst_label": { "type": "string" },
                        "predicate": { "type": "object" },
                        "edge_type": { "type": "string" },
                        "weight_prop": { "type": ["string", "null"] },
                        "max_edges": { "type": ["integer", "null"] }
                    },
                    "required": ["name", "src_label", "dst_label", "predicate", "edge_type"]
                }
            },
            {
                "name": "explain",
                "description": "Explain rule-derived edges between two node keys.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "a": { "type": "string", "minLength": 1 },
                        "b": { "type": "string", "minLength": 1 }
                    },
                    "required": ["a", "b"]
                }
            },
            {
                "name": "stats",
                "description": "Return live node, edge, and rule statistics.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "neighborhood",
                "description": "Traverse the neighborhood of a node key.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" },
                        "depth": { "type": "integer" },
                        "edge_types": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "direction": {
                            "type": "string",
                            "enum": ["out", "in", "both"]
                        }
                    },
                    "required": ["key"]
                }
            },
            {
                "name": "node_info",
                "description": "Return a node's key, label, and properties.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" }
                    },
                    "required": ["key"]
                }
            },
            {
                "name": "node_edges",
                "description": "Return all edges incident on a node key.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" }
                    },
                    "required": ["key"]
                }
            },
            {
                "name": "upsert_entity",
                "description": "Insert or update a node by key. If the key exists, updates the supplied properties. If not, creates a new node with the given label and properties. Useful for agent memory: store or refresh an entity without checking existence first.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string", "description": "Unique node key." },
                        "label": { "type": "string", "description": "Node label (required when creating a new entity)." },
                        "props": {
                            "type": "object",
                            "description": "Properties to set. Values must be scalars (string, number, bool) or arrays of scalars."
                        }
                    },
                    "required": ["key", "props"]
                }
            },
            {
                "name": "find_similar",
                "description": "Two modes: (1) Vector search — provide `vector` (and optionally `field`, `label`, `k`, `min`) to find the k most similar nodes by cosine similarity using the HNSW index when available, brute-force otherwise. (2) Edge traversal — provide `key` (and optionally `edge_type`, `limit`) to return neighbors previously connected by a derived rule edge. Results from mode 2 come only from edges already derived by a VectorSimilar rule.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "vector": {
                            "type": "array",
                            "items": { "type": "number" },
                            "description": "Query embedding vector for vector-similarity search. When present, vector-search mode is used and `key` is ignored."
                        },
                        "field": { "type": "string", "description": "Property field holding the embedding vectors (default: embedding). Used in vector-search mode." },
                        "label": { "type": "string", "description": "Restrict search to nodes with this label. Empty string means all labels. Used in vector-search mode." },
                        "k": { "type": "integer", "description": "Maximum results to return in vector-search mode (default: 10)." },
                        "min": { "type": "number", "description": "Minimum cosine similarity threshold in vector-search mode (default: 0.8)." },
                        "key": { "type": "string", "description": "Source node key for edge-traversal mode." },
                        "edge_type": { "type": "string", "description": "Edge type to filter by in edge-traversal mode (default: SIMILAR)." },
                        "limit": { "type": "integer", "description": "Maximum neighbors to return in edge-traversal mode (default: 10)." }
                    }
                }
            },
            {
                "name": "explain_association",
                "description": "Explain rule-derived associations between two node keys. Returns the rules, edge types, and match scores that connect them. Useful for agent memory: understand why two entities are associated.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "a": { "type": "string", "minLength": 1 },
                        "b": { "type": "string", "minLength": 1 }
                    },
                    "required": ["a", "b"]
                }
            }
        ]
    })
}

fn tool_ok(payload: Js) -> Js {
    json!({
        "content": [{ "type": "text", "text": payload.to_string() }]
    })
}

fn tool_err(message: &str) -> Js {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

fn write_result(writer: &mut impl Write, id: Option<Js>, result: Js) -> io::Result<()> {
    write_json(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id.unwrap_or(Js::Null),
            "result": result
        }),
    )
}

fn write_error(
    writer: &mut impl Write,
    id: Option<Js>,
    code: i64,
    message: &str,
) -> io::Result<()> {
    write_json(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id.unwrap_or(Js::Null),
            "error": { "code": code, "message": message }
        }),
    )
}

fn write_json(writer: &mut impl Write, value: &Js) -> io::Result<()> {
    let s = serde_json::to_string(value).map_err(io::Error::other)?;
    writeln!(writer, "{s}")?;
    writer.flush()
}

// ---------------------------------------------------------------------------
// Tests: MCP tool round-trips via stdio
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_api::{AutoFk, IngestOptions, Predicate, RuleDef, Value};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_dir() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mcp-test-{}-{}", std::process::id(), n))
    }

    /// Open a SharedDb with two Person nodes and one derived SIMILAR edge.
    fn demo_db() -> SharedDb {
        let db = SharedDb::open(&tmp_dir()).expect("open");
        {
            let mut g = db.write();
            let opts = IngestOptions {
                key_field: "id".into(),
                auto_fk: AutoFk::Off,
            };
            // Two people with identical embeddings → will fire SIMILAR rule.
            let people: Vec<BTreeMap<String, Value>> = vec![
                [
                    ("id", Value::Str("alice".into())),
                    ("name", Value::Str("Alice".into())),
                    (
                        "emb",
                        Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
                    ),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
                [
                    ("id", Value::Str("bob".into())),
                    ("name", Value::Str("Bob".into())),
                    (
                        "emb",
                        Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
                    ),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            ];
            g.ingest("Person", people, &opts).expect("ingest");

            // Rule: VectorSimilar on emb → SIMILAR edge (cosine(ident,ident)=1.0 ≥ 0.9).
            g.create_rule(RuleDef {
                name: "sim_emb".into(),
                src_label: "Person".into(),
                dst_label: "Person".into(),
                predicate: Predicate::VectorSimilar {
                    field: "emb".into(),
                    min: 0.9,
                },
                edge_type: "SIMILAR".into(),
                weight_prop: Some("score".into()),
                max_edges: None,
                approximate: false,
                via_label: None,
                via_edge: None,
                via_dir: None,
            })
            .expect("rule");
        }
        db
    }

    fn roundtrip(db: &SharedDb, request: &str) -> Js {
        let input = format!("{request}\n");
        let mut output = Vec::new();
        run_mcp_stdio(db.clone(), input.as_bytes(), &mut output).expect("mcp");
        let s = std::str::from_utf8(&output).expect("utf8");
        serde_json::from_str(s.trim()).expect("json response")
    }

    fn tool_call(db: &SharedDb, id: u64, tool: &str, args: Js) -> Js {
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        });
        roundtrip(db, &req.to_string())
    }

    /// Unwrap the `text` field from a successful tool response.
    fn tool_text(resp: &Js) -> Js {
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("content[0].text");
        serde_json::from_str(text).expect("tool text is json")
    }

    fn is_error(resp: &Js) -> bool {
        resp["result"]["isError"].as_bool().unwrap_or(false)
    }

    // --- existing tools ---

    #[test]
    fn test_tools_list_includes_all_eleven() {
        let db = demo_db();
        let resp = roundtrip(&db, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().expect("name"))
            .collect();
        for expected in &[
            "query",
            "ingest_json",
            "create_rule",
            "explain",
            "stats",
            "neighborhood",
            "node_info",
            "node_edges",
            "upsert_entity",
            "find_similar",
            "explain_association",
        ] {
            assert!(names.contains(expected), "missing tool: {expected}");
        }
        assert_eq!(
            names.len(),
            11,
            "expected exactly 11 tools, got {}",
            names.len()
        );
    }

    #[test]
    fn test_stats_returns_node_count() {
        let db = demo_db();
        let resp = tool_call(&db, 1, "stats", json!({}));
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        assert_eq!(result["nodes_live"], 2);
    }

    #[test]
    fn test_query_runs_cypher() {
        let db = demo_db();
        let resp = tool_call(
            &db,
            1,
            "query",
            json!({ "cypher": "MATCH (n:Person) RETURN n.name ORDER BY n.name" }),
        );
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        // columns + 2 rows
        assert_eq!(result["columns"], json!(["n.name"]));
        assert_eq!(result["rows"].as_array().map(|r| r.len()), Some(2));
    }

    #[test]
    fn test_query_create_is_a_write() {
        let db = SharedDb::open(&tmp_dir()).expect("open");
        let resp = tool_call(
            &db,
            1,
            "query",
            json!({ "cypher": "CREATE (n:L {id: 'k'}) RETURN n" }),
        );
        assert!(
            !is_error(&resp),
            "CREATE via MCP query must succeed: {resp}"
        );
        let stats = tool_text(&tool_call(&db, 2, "stats", json!({})));
        assert_eq!(stats["nodes_live"], 1);
    }

    #[test]
    fn test_ingest_json_inserts_nodes() {
        let db = demo_db();
        let resp = tool_call(
            &db,
            1,
            "ingest_json",
            json!({
                "label": "Person",
                "rows_json": r#"[{"id":"carol","name":"Carol"}]"#,
                "key_field": "id"
            }),
        );
        assert!(!is_error(&resp));
        // Verify node visible via stats
        let stats = tool_text(&tool_call(&db, 2, "stats", json!({})));
        assert_eq!(stats["nodes_live"], 3);
    }

    #[test]
    fn test_node_info_returns_props() {
        let db = demo_db();
        let resp = tool_call(&db, 1, "node_info", json!({ "key": "alice" }));
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        assert_eq!(result["key"], "alice");
        assert_eq!(result["label"], "Person");
        assert_eq!(result["props"]["name"], "Alice");
    }

    #[test]
    fn test_node_edges_returns_edges() {
        let db = demo_db();
        let resp = tool_call(&db, 1, "node_edges", json!({ "key": "alice" }));
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        let edges = result["edges"].as_array().expect("edges");
        assert!(
            !edges.is_empty(),
            "alice should have at least one derived edge"
        );
        // All edges touch alice.
        for e in edges {
            let touches = e["src_key"] == "alice" || e["dst_key"] == "alice";
            assert!(touches, "edge does not touch alice: {e}");
        }
    }

    #[test]
    fn test_neighborhood_traverses_one_hop() {
        let db = demo_db();
        let resp = tool_call(
            &db,
            1,
            "neighborhood",
            json!({ "key": "alice", "depth": 1 }),
        );
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        assert!(result["rows"].as_array().is_some());
    }

    #[test]
    fn test_explain_returns_rule_info() {
        let db = demo_db();
        let resp = tool_call(&db, 1, "explain", json!({ "a": "alice", "b": "bob" }));
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        let arr = result.as_array().expect("explain returns array");
        assert!(!arr.is_empty(), "expected at least one explanation");
        assert_eq!(arr[0]["rule"], "sim_emb");
    }

    #[test]
    fn test_create_rule_backfills() {
        let db = SharedDb::open(&tmp_dir()).expect("open");
        {
            let mut g = db.write();
            let opts = IngestOptions {
                key_field: "id".into(),
                auto_fk: AutoFk::Off,
            };
            let rows: Vec<BTreeMap<String, Value>> = vec![
                [
                    ("id", Value::Str("x".into())),
                    ("tag", Value::Str("a".into())),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
                [
                    ("id", Value::Str("y".into())),
                    ("tag", Value::Str("a".into())),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            ];
            g.ingest("Item", rows, &opts).expect("ingest");
        }
        let resp = tool_call(
            &db,
            1,
            "create_rule",
            json!({
                "name": "same_tag",
                "src_label": "Item",
                "dst_label": "Item",
                "predicate": { "FieldEqual": { "field": "tag" } },
                "edge_type": "SAME_TAG"
            }),
        );
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        assert_eq!(result["ok"], true);
        // Derived edges should now exist.
        let edges_resp = tool_call(&db, 2, "node_edges", json!({ "key": "x" }));
        let edges_result = tool_text(&edges_resp);
        let edges = edges_result["edges"].as_array().expect("edges");
        assert!(
            edges.iter().any(|e| e["edge_type"] == "SAME_TAG"),
            "SAME_TAG edge not found after create_rule"
        );
    }

    // --- new tools ---

    #[test]
    fn test_upsert_entity_creates_new_node() {
        let db = demo_db();
        let resp = tool_call(
            &db,
            1,
            "upsert_entity",
            json!({
                "key": "carol",
                "label": "Person",
                "props": { "name": "Carol", "age": 30 }
            }),
        );
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        assert_eq!(result["ok"], true);
        assert_eq!(result["created"], true);
        assert_eq!(result["key"], "carol");
        // Verify node exists
        let info = tool_text(&tool_call(&db, 2, "node_info", json!({ "key": "carol" })));
        assert_eq!(info["props"]["name"], "Carol");
    }

    #[test]
    fn test_upsert_entity_updates_existing_node() {
        let db = demo_db();
        let resp = tool_call(
            &db,
            1,
            "upsert_entity",
            json!({
                "key": "alice",
                "props": { "name": "Alice Updated" }
            }),
        );
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        assert_eq!(result["ok"], true);
        assert_eq!(result["created"], false);
        assert_eq!(result["updated_fields"], 1);
        // Verify prop changed
        let info = tool_text(&tool_call(&db, 2, "node_info", json!({ "key": "alice" })));
        assert_eq!(info["props"]["name"], "Alice Updated");
    }

    #[test]
    fn test_upsert_entity_missing_label_on_create_is_error() {
        let db = demo_db();
        let resp = tool_call(
            &db,
            1,
            "upsert_entity",
            json!({ "key": "new-node", "props": { "x": 1 } }),
        );
        assert!(is_error(&resp), "should error without label for new node");
    }

    #[test]
    fn test_find_similar_returns_similar_edges() {
        let db = demo_db();
        let resp = tool_call(
            &db,
            1,
            "find_similar",
            json!({ "key": "alice", "edge_type": "SIMILAR" }),
        );
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        assert_eq!(result["key"], "alice");
        assert_eq!(result["edge_type"], "SIMILAR");
        let similar = result["similar"].as_array().expect("similar array");
        assert!(!similar.is_empty(), "expected SIMILAR neighbors for alice");
        assert_eq!(similar[0]["neighbor_key"], "bob");
    }

    #[test]
    fn test_find_similar_limit_respected() {
        let db = demo_db();
        let resp = tool_call(
            &db,
            1,
            "find_similar",
            json!({ "key": "alice", "edge_type": "SIMILAR", "limit": 0 }),
        );
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        let similar = result["similar"].as_array().expect("similar array");
        assert_eq!(similar.len(), 0);
    }

    /// When `min` is omitted from a vector-mode find_similar call, the server
    /// must apply the spec default of 0.8.  A node whose cosine similarity to
    /// the query is 0.0 (orthogonal) must not appear in the results.
    #[test]
    fn test_find_similar_vector_default_min_is_0_8() {
        let db = SharedDb::open(&tmp_dir()).expect("open");
        {
            let mut g = db.write();
            // close: [1,0] → cosine 1.0 with query [1,0] (above 0.8)
            g.insert_node(
                "Item",
                "close",
                vec![(
                    "emb".into(),
                    Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
                )],
            )
            .unwrap();
            // far: [0,1] → cosine 0.0 with query [1,0] (below 0.8, must be excluded)
            g.insert_node(
                "Item",
                "far",
                vec![(
                    "emb".into(),
                    Value::List(vec![Value::Float(0.0), Value::Float(1.0)]),
                )],
            )
            .unwrap();
        }

        // No `min` in the request — must default to 0.8.
        let resp = tool_call(
            &db,
            1,
            "find_similar",
            json!({
                "vector": [1.0, 0.0],
                "field": "emb",
                "label": "Item",
                "k": 10
            }),
        );
        assert!(!is_error(&resp), "vector search must not error");
        let result = tool_text(&resp);
        let results = result["results"].as_array().expect("results array");

        let keys: Vec<&str> = results.iter().filter_map(|r| r["key"].as_str()).collect();
        assert!(
            keys.contains(&"close"),
            "close node (sim=1.0) must be included"
        );
        assert!(
            !keys.contains(&"far"),
            "far node (sim=0.0) must be excluded by default min=0.8"
        );
    }

    #[test]
    fn test_explain_association_same_as_explain() {
        let db = demo_db();
        let explain = tool_text(&tool_call(
            &db,
            1,
            "explain",
            json!({ "a": "alice", "b": "bob" }),
        ));
        let assoc = tool_text(&tool_call(
            &db,
            2,
            "explain_association",
            json!({ "a": "alice", "b": "bob" }),
        ));
        // Both tools return identical results.
        assert_eq!(explain, assoc);
    }
}
