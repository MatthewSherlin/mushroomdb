//! MCP server: JSON-RPC 2.0 over newline-delimited stdio.
//!
//! Framing is **one JSON object per line**. LSP-style `Content-Length`
//! headers are not accepted — a header line is a parse error (`-32700`)
//! and the loop continues. Blank lines are skipped.
//!
//! # Methods
//!
//! - `initialize` — `protocolVersion` `"2024-11-05"`, `capabilities.tools`,
//!   `serverInfo.name` `"graph-db"`
//! - `notifications/initialized` — ignored
//! - `tools/list` — the seven tools below, each with a JSON Schema
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

use crate::json::{node_edges_json, node_info_json, params_from_json, result_set_json};
use core_api::{AutoFk, Dir, GraphError, IngestOptions, SharedDb};
use serde_json::{json, Value as Js};
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
        "explain" => tool_explain(db, args),
        "stats" => tool_stats(db),
        "neighborhood" => tool_neighborhood(db, args),
        "node_info" => tool_node_info(db, args),
        "node_edges" => tool_node_edges(db, args),
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
    let rs = {
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
    let report = {
        let mut g = db.write();
        g.ingest_json(label, rows_json, &opts)
    };
    match report {
        Ok(r) => match serde_json::to_value(&r) {
            Ok(v) => CallOutcome::ToolOk(v),
            Err(e) => CallOutcome::ToolErr(e.to_string()),
        },
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
        "serverInfo": { "name": "graph-db" }
    })
}

fn tools_list() -> Js {
    json!({
        "tools": [
            {
                "name": "query",
                "description": "Run a Cypher query against the graph.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cypher": { "type": "string", "description": "Cypher query text." },
                        "params": {
                            "type": "object",
                            "description": "Named JSON-scalar query parameters."
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
                        "auto_fk_suffix": { "type": "string" }
                    },
                    "required": ["label", "rows_json"]
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
