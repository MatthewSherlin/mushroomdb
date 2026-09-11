//! MCP server: JSON-RPC 2.0 over newline-delimited stdio.
//!
//! Framing is **one JSON object per line**. LSP-style `Content-Length`
//! headers are not accepted — a header line is a parse error (`-32700`)
//! and the loop continues. Blank lines are skipped.
//!
//! # Methods
//!
//! - `initialize` — `protocolVersion` `"2024-11-05"`, `capabilities.tools`,
//!   `serverInfo.name` `"mushroomdb"`, `serverInfo.version` (crate version)
//! - `notifications/initialized` — ignored
//! - `tools/list` — the default listing follows the store the server opened
//!   (see [`Surface`]): a store a repository was ingested into lists three —
//!   `explore`, `query`, `stats` — and any other store lists the fifteen of
//!   [`ASSOCIATION_TOOLS`], the tools that answer a question about an entity
//!   graph, in that order. Graph-tool descriptions carry the
//!   prefix `Advanced: ` so a host ranking tools by description puts the task
//!   tools in front. `mushroomdb mcp --all-tools` lists all twenty-seven; the
//!   rest are callable either way, just not advertised
//! - `tools/call` — dispatch; success for a graph tool is
//!   `{content:[{type:"text", text:<json string>}]}`, and for a task tool one
//!   text block holding the rendered digest — or, with `json: true`, the
//!   serialised report. No task tool returns `structuredContent`
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
    edge_history_result_json, node_history_json, node_info_json, params_from_json,
    parse_ingest_edges, result_set_json, rule_def_from_json,
};
use core_api::{
    json_to_rows, json_to_value, AutoFk, GraphError, IngestOptions, MaskMode, NodeMask, SharedDb,
    Value,
};
use serde_json::{json, Value as Js};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

/// Run the MCP loop until `reader` hits EOF.
///
/// `db_dir` is where the store lives on disk. `mushroomdb mcp <db>` passes it;
/// a caller that has only a handle passes `None`, and the one tool that needs a
/// path — `sync`, which re-runs this binary against the store — reports that it
/// cannot run rather than guessing one.
pub fn run_mcp_stdio(
    db: SharedDb,
    db_dir: Option<PathBuf>,
    reader: impl BufRead,
    writer: impl Write,
) -> io::Result<()> {
    run_mcp_stdio_with(db, db_dir, false, reader, writer)
}

/// [`run_mcp_stdio`], with the tool list chosen by the caller.
///
/// `all_tools` false lists what the store's [`Surface`] names — three on a
/// code graph, fifteen on a memory store; true lists all twenty-seven. Either
/// way every tool remains callable — the flag decides what is advertised, not
/// what is served.
///
/// The surface is read once, here, rather than per `tools/list`: a store does
/// not become a code graph half way through a session, and a listing that
/// changed under a host that caches it would be worse than one that is merely
/// stale.
pub fn run_mcp_stdio_with(
    db: SharedDb,
    db_dir: Option<PathBuf>,
    all_tools: bool,
    mut reader: impl BufRead,
    mut writer: impl Write,
) -> io::Result<()> {
    let surface = surface_of(&db);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        match std::str::from_utf8(&buf) {
            Ok(s) if s.trim().is_empty() => continue,
            Ok(s) => handle_line(
                &db,
                db_dir.as_deref(),
                all_tools,
                surface,
                s.trim(),
                &mut writer,
            )?,
            Err(_) => write_error(&mut writer, None, -32700, "Parse error")?,
        }
    }
}

fn handle_line(
    db: &SharedDb,
    db_dir: Option<&Path>,
    all_tools: bool,
    surface: Surface,
    line: &str,
    writer: &mut impl Write,
) -> io::Result<()> {
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
                write_result(writer, id, tools_list(all_tools, surface))?;
            }
        }
        "tools/call" => {
            if is_request {
                match dispatch_call(db, db_dir, obj.get("params")) {
                    CallOutcome::Protocol { code, message } => {
                        write_error(writer, id, code, &message)?;
                    }
                    CallOutcome::ToolOk(payload) => {
                        write_result(writer, id, tool_ok(payload))?;
                    }
                    CallOutcome::TaskOk { text } => {
                        write_result(writer, id, task_ok(&text))?;
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

pub(crate) enum CallOutcome {
    Protocol {
        code: i64,
        message: String,
    },
    /// A graph tool's JSON payload, returned as a JSON string in `content`.
    ToolOk(Js),
    /// A task tool's answer, as text and nothing else: the rendered digest, or
    /// the serialised report when the call passed `json: true`.
    TaskOk {
        text: String,
    },
    ToolErr(String),
}

fn dispatch_call(db: &SharedDb, db_dir: Option<&Path>, params: Option<&Js>) -> CallOutcome {
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
    // The repository task tools first, in the order `tools/list` advertises.
    if let Some(outcome) = crate::mcp_tasks::dispatch(db, db_dir, name, args) {
        return outcome;
    }
    match name {
        "query" => tool_query(db, args),
        "ingest_json" => tool_ingest(db, args),
        "create_rule" => tool_create_rule(db, args),
        "explain" => tool_explain(db, args),
        "stats" => tool_stats(db),
        "node_info" => tool_node_info(db, args),
        "upsert_entity" => tool_upsert_entity(db, args),
        "find_similar" => tool_find_similar(db, args),
        "hybrid_search" => tool_hybrid_search(db, args),
        "node_history" => tool_node_history(db, args),
        "edge_history" => tool_edge_history(db, args),
        "was_linked" => tool_was_linked(db, args),
        "rename_node" => tool_rename_node(db, args),
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

    // Two ways to ask the same restricted question: a `role` names one the
    // store already defines, a `mask` writes the allow-list out by hand. Both
    // route to `query_masked` (read-only). Passing both is not a merge of the
    // two — it is a caller that has not decided which restriction applies, so
    // it is refused rather than silently resolved one way.
    let role = match args.get("role") {
        None | Some(Js::Null) => None,
        Some(Js::String(s)) if !s.is_empty() => Some(s.as_str()),
        Some(_) => return CallOutcome::ToolErr("role must be a non-empty string".into()),
    };
    let mask_keys = match args.get("mask") {
        None => None,
        Some(v) => match mask_key_list(v) {
            Ok(keys) => Some(keys),
            Err(e) => return CallOutcome::ToolErr(e),
        },
    };
    if role.is_some() && mask_keys.is_some() {
        return CallOutcome::ToolErr("pass role or mask, not both".into());
    }

    if role.is_some() || mask_keys.is_some() {
        let stub_hidden = args
            .get("stub_hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let g = db.read();
        let mask = match (role, &mask_keys) {
            (Some(role), _) => match g.mask_for_role(role) {
                Ok(m) => m,
                // The one error a caller can fix by rereading `roles.json`,
                // told apart from a store whose roles never loaded at all.
                Err(GraphError::KeyNotFound { .. }) => {
                    return CallOutcome::ToolErr(format!("unknown role '{role}'"))
                }
                Err(e) => return CallOutcome::ToolErr(graph_err_msg(e)),
            },
            (None, Some(keys)) => NodeMask::from_keys(&*g, keys.iter().map(String::as_str)),
            (None, None) => unreachable!("one of the two is Some in this branch"),
        };
        let mask = if stub_hidden {
            mask.with_mode(MaskMode::Stub)
        } else {
            mask
        };
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

/// A `mask` argument as a key list. `Err` when it is anything but an array of
/// strings — including `null`, which is a caller that meant to pass one.
fn mask_key_list(mask: &Js) -> Result<Vec<String>, String> {
    let arr = mask
        .as_array()
        .ok_or_else(|| "mask must be an array of strings".to_string())?;
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| "mask must be an array of strings".to_string())
        })
        .collect()
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
    // Parse the optional mask once — it applies to both vector and edge paths.
    // An invalid mask value (non-array or non-string element) fails closed.
    let mask_keys: Option<Vec<String>> = if let Some(mask_val) = args.get("mask") {
        match mask_val.as_array() {
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
                Some(ks)
            }
            None => return CallOutcome::ToolErr("mask must be an array of strings".into()),
        }
    } else {
        None
    };

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
        let label_str = args.get("label").and_then(Js::as_str).unwrap_or("");
        let label = if label_str.is_empty() {
            None
        } else {
            Some(label_str)
        };
        let k = args
            .get("k")
            .and_then(Js::as_u64)
            .map(|n| n as usize)
            .unwrap_or(10);
        let min = args.get("min").and_then(Js::as_f64).unwrap_or(0.8);

        let hits = {
            let g = db.read();
            if let Some(ref keys) = mask_keys {
                let node_mask = NodeMask::from_keys(&*g, keys.iter().map(String::as_str));
                g.find_similar_vector_masked(field, label, &q, k, min, &node_mask)
            } else {
                g.find_similar_vector(field, label, &q, k, min)
            }
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

    // When a mask is present, a hidden query key behaves identically to a
    // nonexistent key — we do not confirm its existence.
    if let Some(ref mask) = mask_keys {
        let mask_set: std::collections::HashSet<&str> = mask.iter().map(String::as_str).collect();
        if !mask_set.contains(key) {
            return CallOutcome::ToolErr(graph_err_msg(GraphError::KeyNotFound {
                key: key.into(),
            }));
        }
        let out = {
            let g = db.read();
            g.node_edges(key)
        };
        return match out {
            Ok(edges) => {
                let similar: Vec<Js> = edges
                    .iter()
                    .filter(|e| e.edge_type == edge_type)
                    .filter(|e| {
                        // Keep only edges where the neighbor is also visible.
                        let neighbor_key = if e.src_key == key {
                            &e.dst_key
                        } else {
                            &e.src_key
                        };
                        mask_set.contains(neighbor_key.as_str())
                    })
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
        };
    }

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

fn tool_hybrid_search(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(query_text) = args.get("query_text").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing required field: query_text".into());
    };
    let Some(text_field) = args.get("text_field").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing required field: text_field".into());
    };

    let vector_field = args
        .get("vector_field")
        .and_then(Js::as_str)
        .unwrap_or("embedding");
    let label = args.get("label").and_then(Js::as_str);
    let k = args
        .get("k")
        .and_then(Js::as_u64)
        .map(|n| n as usize)
        .unwrap_or(10);

    let query_vec: Vec<f64> = args
        .get("vector")
        .and_then(Js::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect())
        .unwrap_or_default();

    let hits = {
        let g = db.read();
        g.search_hybrid(text_field, query_text, vector_field, &query_vec, label, k)
    };

    let results: Vec<Js> = hits
        .into_iter()
        .map(|(key, score)| json!({ "key": key, "score": score }))
        .collect();

    CallOutcome::ToolOk(json!({
        "query_text": query_text,
        "text_field": text_field,
        "vector_field": vector_field,
        "label": label,
        "k": k,
        "results": results
    }))
}

fn tool_node_history(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(key) = args.get("key").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing key".into());
    };
    let g = db.read();
    let result = match g.node_history(key) {
        Ok(e) => e,
        Err(e) => return CallOutcome::ToolErr(graph_err_msg(e)),
    };
    CallOutcome::ToolOk(node_history_json(key, &result))
}

fn tool_edge_history(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(a) = args.get("a").and_then(Js::as_str).filter(|s| !s.is_empty()) else {
        return CallOutcome::ToolErr("missing a".into());
    };
    let Some(b) = args.get("b").and_then(Js::as_str).filter(|s| !s.is_empty()) else {
        return CallOutcome::ToolErr("missing b".into());
    };
    let result = {
        let g = db.read();
        g.edge_history(a, b)
    };
    match result {
        Ok(hr) => CallOutcome::ToolOk(edge_history_result_json(a, b, &hr)),
        Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
    }
}

fn tool_was_linked(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(a) = args.get("a").and_then(Js::as_str).filter(|s| !s.is_empty()) else {
        return CallOutcome::ToolErr("missing a".into());
    };
    let Some(b) = args.get("b").and_then(Js::as_str).filter(|s| !s.is_empty()) else {
        return CallOutcome::ToolErr("missing b".into());
    };
    let Some(edge_type) = args
        .get("edge_type")
        .and_then(Js::as_str)
        .filter(|s| !s.is_empty())
    else {
        return CallOutcome::ToolErr("missing edge_type".into());
    };
    let at_commit = match args.get("at_commit").and_then(Js::as_u64) {
        Some(n) => n,
        None => return CallOutcome::ToolErr("missing or invalid at_commit".into()),
    };
    let result = {
        let g = db.read();
        g.was_linked(a, b, edge_type, at_commit)
    };
    match result {
        Ok(linked) => CallOutcome::ToolOk(json!({
            "a": a,
            "b": b,
            "edge_type": edge_type,
            "at_commit": at_commit,
            "linked": linked,
        })),
        Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
    }
}

fn tool_rename_node(db: &SharedDb, args: &Js) -> CallOutcome {
    let Some(old_key) = args.get("old_key").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing old_key".into());
    };
    let Some(new_key) = args.get("new_key").and_then(Js::as_str) else {
        return CallOutcome::ToolErr("missing new_key".into());
    };
    let mut g = db.write();
    match g.rename_node(old_key, new_key) {
        Ok(()) => CallOutcome::ToolOk(json!({
            "ok": true,
            "old_key": old_key,
            "new_key": new_key,
        })),
        Err(e) => CallOutcome::ToolErr(graph_err_msg(e)),
    }
}

pub(crate) fn graph_err_msg(e: GraphError) -> String {
    match e {
        GraphError::QueryError { detail } | GraphError::IngestError { detail } => detail,
        other => other.to_string(),
    }
}

fn initialize_result() -> Js {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "mushroomdb", "version": env!("CARGO_PKG_VERSION") }
    })
}

/// The prefix every graph tool's description carries.
///
/// A host that ranks tools by their description now has one signal that the
/// repository task tools are the ones to reach for first, and that everything
/// under this prefix is the lower-level surface beneath them.
const ADVANCED_PREFIX: &str = "Advanced: ";

/// The three a code-graph store advertises: one tool to find, one to ask an
/// arbitrary question, one to size the store.
///
/// `ingest_json` is not among them. A store built by `ingest-git` is written by
/// `sync` and `touch`, not by an assistant bulk-loading rows into it, and the
/// tool that is never the right one on this surface is the one worth not
/// listing.
pub const CODE_GRAPH_TOOLS: [&str; 3] = ["explore", "query", "stats"];

/// The fifteen a memory store advertises, in the order it lists them.
///
/// A store with no repository in it used to be handed the code door's own task
/// tools — `map`, `context`, `impact`, `owners`, `why`, `sync` — which answer
/// from a code graph there is none of, plus `ingest_json`. Six of the eleven
/// names an assistant found answered from a repository the store did not
/// hold. These are
/// the questions an entity graph *can* answer: what is there (`query` — now
/// with a `role`), why two things are associated, what is around a node, what
/// it is and what it is joined to, whether a link held at a commit and when it
/// changed, what is like it, and what was written down about it.
///
/// Listing order is ranking: a host that defers schemas shows this list in
/// order, so the two questions this door exists for come first.
///
/// `edges_at` and `what_if` are the two the first association benchmark run
/// showed missing: a run asked what a node's relationships were at a past
/// commit and spent twenty to sixty-seven turns replaying `edge_history` for
/// it, and had no way at all to ask what a change would do. They sit after
/// `was_linked`, which is the narrowest form of the same time question.
///
/// The code task tools stay served on a memory store, as these stay served on
/// a code-graph one — [`tools_list`] decides what is *advertised*, never what
/// is answered.
pub const ASSOCIATION_TOOLS: [&str; 15] = [
    "query",
    "explain_association",
    "neighborhood",
    "node_info",
    "node_edges",
    "was_linked",
    "edges_at",
    "what_if",
    "node_history",
    "edge_history",
    "find_similar",
    "hybrid_search",
    "remember",
    "recall",
    "stats",
];

/// Which door a store is: which default tool list it gets.
///
/// Decided from the store the server opened, once, at startup — not from an
/// install flag — so one `.mcp.json` serves both and neither has to be
/// configured for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    /// A repository was ingested into this store: the `GitSync` marker is
    /// there, and `explore` has a code graph to explore.
    CodeGraph,
    /// Any other store, including an empty one: the fifteen-tool association
    /// surface, where `explore` would have nothing to answer from.
    Memory,
}

impl Surface {
    /// The tools a default `tools/list` on this surface advertises, in the
    /// order it advertises them.
    fn listing(self) -> &'static [&'static str] {
        match self {
            Surface::CodeGraph => &CODE_GRAPH_TOOLS,
            Surface::Memory => &ASSOCIATION_TOOLS,
        }
    }
}

/// The surface the store `db` holds: [`Surface::CodeGraph`] when it carries the
/// `GitSync` marker `ingest-git` writes, [`Surface::Memory`] otherwise.
fn surface_of(db: &SharedDb) -> Surface {
    let ingested = {
        let g = db.read();
        g.has_node(crate::mcp_tasks::SYNC_KEY)
    };
    if ingested {
        Surface::CodeGraph
    } else {
        Surface::Memory
    }
}

/// The tools `tools/list` advertises: the fourteen task tools, then the
/// graph tools with their descriptions prefixed.
///
/// `all` false — the default — lists what `surface` names, **in the order that
/// surface names it**: three on a code graph, fifteen on a memory store. The
/// order is the point. A host that defers tool schemas makes a model search
/// for them, and the list it searches is read top-down, so each surface ranks
/// its own tools rather than inheriting the task-tools-then-graph-tools order
/// that only the code door has a reason for.
///
/// `all` true lists all twenty-seven in that established order whichever store
/// this is, which is what `mushroomdb mcp --all-tools` runs and what the
/// published server card documents: a caller that asked for everything asked
/// for the whole surface, not for one door's ranking of it.
///
/// Either way every tool stays callable: the flag and the surface decide what
/// is advertised, not what is served.
fn tools_list(all: bool, surface: Surface) -> Js {
    let mut served: Vec<Js> = crate::mcp_tasks::task_tools();
    for mut tool in graph_tools() {
        if let Some(d) = tool.get("description").and_then(Js::as_str) {
            let prefixed = format!("{ADVANCED_PREFIX}{d}");
            tool["description"] = Js::String(prefixed);
        }
        served.push(tool);
    }
    if all {
        return json!({ "tools": served });
    }
    let listing = surface.listing();
    let mut tools: Vec<Js> = Vec::with_capacity(listing.len());
    for name in listing {
        let Some(tool) = served
            .iter()
            .find(|t| t.get("name").and_then(Js::as_str) == Some(*name))
        else {
            debug_assert!(false, "{surface:?} lists {name}, which is not served");
            continue;
        };
        tools.push(tool.clone());
    }
    json!({ "tools": tools })
}

/// The thirteen graph tools, in the order they have always been listed, with
/// their descriptions unprefixed. [`tools_list`] adds the prefix.
fn graph_tools() -> Vec<Js> {
    let Js::Array(tools) = json!([
            {
                "name": "query",
                "description": "Who may see this, and anything else one pattern can answer — run a Cypher query (read or write) against the graph. Pass 'role' to answer as one of the store's roles: only the nodes that role may see, writes refused. 'mask' is the same restriction written out as an explicit key allow-list. Cypher dialect: MATCH/WHERE/RETURN, CREATE, MERGE, SET, DELETE, with $named parameters in 'params'. A node's key and label read as properties (n.key, n.label) or as key(n)/labels(n). One MATCH takes comma-separated patterns that share variables — MATCH (t)-[:A]->(c), (t)-[:B]->(c) is the intersection of both, and count(DISTINCT t) after WITH counts each t once. WHERE takes STARTS WITH, ENDS WITH, CONTAINS, IN, and a list subscript (n.location[0]) — which is null when the index is out of range, the property is not a list, or the index is not an integer, so a subscript never errors and never matches.",
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
                        },
                        "role": {
                            "type": "string",
                            "description": "Answer as this role from the store's roles: only the nodes it may see."
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
                "description": "How should this kind of relationship be derived from now on — declare a rule (RuleDef JSON) and the engine maintains its edges as the data changes. Propose it and show the edges it would derive before creating one.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "src_label": { "type": "string" },
                        "dst_label": { "type": "string" },
                        "predicate": { "type": "object" },
                        "edge_type": { "type": "string" },
                        "weight_prop": {
                            "type": ["string", "null"],
                            "description": "Edge property that stores the score (default: weight)."
                        },
                        "max_edges": { "type": ["integer", "null"] }
                    },
                    "required": ["name", "src_label", "dst_label", "predicate", "edge_type"]
                }
            },
            {
                "name": "explain",
                "description": "Why are A and B related, as a raw array — the same rule-derived edges explain_association renders, for a caller that wants the JSON without asking.",
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
                "description": "How big is this store — live node, edge and rule counts, plus `history_floor`, the oldest commit history still reaches (0 when nothing has been pruned).",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "node_info",
                "description": "What is K — its label and every property it holds.",
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
                "description": "Record what is now true about K — insert or update a node by key. If the key exists, updates the supplied properties. If not, creates a new node with the given label and properties. Useful for agent memory: store or refresh an entity without checking existence first.",
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
                "description": "What is most like this — two modes: (1) Vector search — provide `vector` (and optionally `field`, `label`, `k`, `min`) to find the k most similar nodes by cosine similarity using the HNSW index when available, brute-force otherwise. (2) Edge traversal — provide `key` (and optionally `edge_type`, `limit`) to return neighbors previously connected by a derived rule edge. Results from mode 2 come only from edges already derived by a VectorSimilar rule. In both modes, the optional `mask` array limits visibility: hidden nodes never appear in results, and a hidden query key in edge mode behaves identically to a nonexistent key.",
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
                        "mask": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional node key allow-list for vector-search mode. When present, only nodes whose key appears in this list are eligible for results. Hidden nodes are excluded before k-truncation so callers still receive up to k visible hits. Unknown keys are silently ignored."
                        },
                        "key": { "type": "string", "description": "Source node key for edge-traversal mode." },
                        "edge_type": { "type": "string", "description": "Edge type to filter by in edge-traversal mode (default: SIMILAR)." },
                        "limit": { "type": "integer", "description": "Maximum neighbors to return in edge-traversal mode (default: 10)." }
                    }
                }
            },
            {
                "name": "hybrid_search",
                "description": "What matches these words and this vector at once — Reciprocal Rank Fusion (RRF) over fulltext + vector results. Provide `query_text` and `text_field` for the fulltext leg. Optionally provide `vector` (embedding array) and `vector_field` (default: embedding) for the vector leg; omitting `vector` gives text-only ranking through the same RRF path. `label` restricts the vector search to nodes with that label (required for brute-force; omit to rely on HNSW rules). `k` controls result count (default: 10). RRF constant is fixed at 60; scores are 1/(60+rank) summed over lists a node appears in.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query_text": { "type": "string", "description": "Fulltext query string." },
                        "text_field": { "type": "string", "description": "Property field to search with fulltext." },
                        "vector": {
                            "type": "array",
                            "items": { "type": "number" },
                            "description": "Query embedding vector. Omit for text-only ranking."
                        },
                        "vector_field": { "type": "string", "description": "Property field holding embedding vectors (default: embedding)." },
                        "label": { "type": "string", "description": "Restrict vector search to nodes with this label. Required when relying on brute-force (no HNSW rule covers the field). If omitted, the vector leg always returns empty results (no rule-created HNSW index covers the unlabeled path); ranking is text-only in that case." },
                        "k": { "type": "integer", "description": "Maximum results to return (default: 10)." }
                    },
                    "required": ["query_text", "text_field"]
                }
            },
            {
                "name": "node_history",
                "description": "What has happened to K — every recorded change to one node, newest last. Events include NodeInserted, PropSet, PropRemoved, EdgeAdded, EdgeRemoved, and NodeDeleted. The response includes `total_commits` (the horizon upper bound) and `horizon`, the oldest commit still retained; events before it are gone. History is WAL-scoped — pre-snapshot commits are not visible.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string", "description": "Node key to look up." }
                    },
                    "required": ["key"]
                }
            },
            {
                "name": "edge_history",
                "description": "When did A and B become linked, and when did it break — the full add/retract lifecycle for every edge between the two keys. Includes derived (rule-attributed) edges via DerivedEdgeAdded/DerivedEdgeRetracted WAL markers. The response includes `total_commits` (the horizon upper bound) and `horizon`, the oldest commit still retained; events before it are gone.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "a": { "type": "string", "minLength": 1, "description": "First node key." },
                        "b": { "type": "string", "minLength": 1, "description": "Second node key." }
                    },
                    "required": ["a", "b"]
                }
            },
            {
                "name": "was_linked",
                "description": "Were A and B linked at commit C — whether an edge of `edge_type` existed between the two keys (either direction) at that WAL commit. Returns an error when `at_commit` is outside the retained horizon (`horizon..total_commits`).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "a": { "type": "string", "minLength": 1, "description": "First node key." },
                        "b": { "type": "string", "minLength": 1, "description": "Second node key." },
                        "edge_type": { "type": "string", "minLength": 1, "description": "Edge type to check." },
                        "at_commit": { "type": "integer", "minimum": 0, "description": "0-based WAL commit index to query." }
                    },
                    "required": ["a", "b", "edge_type", "at_commit"]
                }
            },
            {
                "name": "rename_node",
                "description": "Rename K — the key changes and nothing else does. The dense id and all edges/properties remain stable. Returns 404 if `old_key` does not exist, 409 if `new_key` is already taken.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "old_key": { "type": "string", "minLength": 1, "description": "Current node key." },
                        "new_key": { "type": "string", "minLength": 1, "description": "Desired new node key." }
                    },
                    "required": ["old_key", "new_key"]
                }
            }
    ]) else {
        unreachable!("the literal above is an array")
    };
    tools
}

fn tool_ok(payload: Js) -> Js {
    json!({
        "content": [{ "type": "text", "text": payload.to_string() }]
    })
}

/// A task tool's result: one text block, and no `structuredContent`.
///
/// The report used to ride along beside the digest, repeating it verbatim
/// under a `text` key. Nothing bound it — no task tool declares an
/// `outputSchema` — and it tripled the size of every reply, so a caller that
/// wants the numbers now asks for them with `json: true` and gets the report
/// *as* the text.
fn task_ok(text: &str) -> Js {
    json!({
        "content": [{ "type": "text", "text": text }]
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
        let d = std::env::temp_dir().join(format!("mcp-test-{}-{}", std::process::id(), n));
        // These stores are never cleaned up, so a process id the OS hands out
        // again lands on a previous run's data and every assertion about counts
        // fails. `tests/mcp.rs::tmp` already clears its path for this reason.
        let _ = std::fs::remove_dir_all(&d);
        d
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
        roundtrip_with(db, false, request)
    }

    fn roundtrip_with(db: &SharedDb, all_tools: bool, request: &str) -> Js {
        let input = format!("{request}\n");
        let mut output = Vec::new();
        run_mcp_stdio_with(db.clone(), None, all_tools, input.as_bytes(), &mut output)
            .expect("mcp");
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

    fn tool_err_text(resp: &Js) -> String {
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    // --- existing tools ---

    #[test]
    fn test_tools_list_includes_all_expected() {
        let db = demo_db();
        let resp = roundtrip_with(
            &db,
            true,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().expect("name"))
            .collect();
        for expected in &[
            // The fourteen task tools, first and in order.
            "explore",
            "map",
            "context",
            "impact",
            "owners",
            "why",
            "explain_association",
            "node_edges",
            "neighborhood",
            "edges_at",
            "what_if",
            "recall",
            "remember",
            "sync",
            // The thirteen graph tools.
            "query",
            "ingest_json",
            "create_rule",
            "explain",
            "stats",
            "node_info",
            "upsert_entity",
            "find_similar",
            "hybrid_search",
            "node_history",
            "edge_history",
            "was_linked",
            "rename_node",
        ] {
            assert!(names.contains(expected), "missing tool: {expected}");
        }
        assert_eq!(
            names.len(),
            27,
            "expected exactly 27 tools, got {}",
            names.len()
        );
        assert_eq!(
            &names[..14],
            [
                "explore",
                "map",
                "context",
                "impact",
                "owners",
                "why",
                "explain_association",
                "node_edges",
                "neighborhood",
                "edges_at",
                "what_if",
                "recall",
                "remember",
                "sync"
            ],
            "the task tools come first, in order"
        );
        assert_eq!(names[14], "query", "the graph tools follow them");
    }

    /// Binding: on a store no repository was ingested into, the default
    /// listing is the fifteen association tools, in [`ASSOCIATION_TOOLS`]
    /// order, and nothing else.
    #[test]
    fn tools_list_defaults_to_fifteen_on_a_memory_store() {
        let db = demo_db();
        let resp = roundtrip(&db, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ASSOCIATION_TOOLS.to_vec());
    }

    /// Binding: [`ASSOCIATION_TOOLS`] is a surface of its own, not the code
    /// door's list with a name changed.
    ///
    /// It keeps the two task tools an entity store can answer with — the notes
    /// it wrote and the notes it kept — and none of the seven that read a code
    /// graph there is none of. Every name in it is served.
    #[test]
    fn the_association_surface_is_entity_tools_only() {
        for kept in ["remember", "recall", "explain_association"] {
            assert!(
                ASSOCIATION_TOOLS.contains(&kept),
                "{kept} answers on an entity graph and must be listed"
            );
        }
        for code_only in [
            "explore", "map", "context", "impact", "owners", "why", "sync",
        ] {
            assert!(
                !ASSOCIATION_TOOLS.contains(&code_only),
                "{code_only} reads a code graph and must not be listed on a memory store"
            );
        }
        let served: Vec<String> = crate::mcp_tasks::task_tools()
            .iter()
            .chain(graph_tools().iter())
            .filter_map(|t| t.get("name").and_then(Js::as_str))
            .map(str::to_string)
            .collect();
        for name in ASSOCIATION_TOOLS {
            assert!(
                served.iter().any(|s| s == name),
                "{name} is listed but not served"
            );
        }
        assert!(
            CODE_GRAPH_TOOLS.contains(&"explore"),
            "and `explore` is the task tool the other surface lists"
        );
    }

    /// Binding: the same server on a store carrying the `GitSync` marker lists
    /// three. One tool to find, one to query, one to size the store.
    #[test]
    fn tools_list_is_three_tools_on_a_code_graph_store() {
        let db = demo_db();
        db.write()
            .insert_node(
                "GitSync",
                crate::mcp_tasks::SYNC_KEY,
                vec![("id".into(), Value::Str(crate::mcp_tasks::SYNC_KEY.into()))],
            )
            .expect("marker");
        let resp = roundtrip(&db, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["explore", "query", "stats"]);
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

    /// Binding: `node_edges` groups by edge type and names the rule and score
    /// behind each derived edge, in the report as in the digest.
    #[test]
    fn test_node_edges_returns_edges() {
        let db = demo_db();
        let resp = tool_call(
            &db,
            1,
            "node_edges",
            json!({ "key": "alice", "json": true }),
        );
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        assert_eq!(result["key"], "alice");
        let types = result["types"].as_array().expect("types");
        assert!(
            !types.is_empty(),
            "alice should have at least one edge type"
        );
        let similar = types
            .iter()
            .find(|t| t["edge_type"] == "SIMILAR")
            .expect("the rule's edge type");
        // A symmetric rule derives the edge both ways, and both are listed
        // with the direction that tells them apart.
        assert_eq!(similar["count"], json!(2));
        let edges = similar["edges"].as_array().expect("edges");
        let dirs: Vec<&str> = edges
            .iter()
            .map(|e| e["direction"].as_str().expect("direction"))
            .collect();
        assert!(dirs.contains(&"out") && dirs.contains(&"in"), "{similar}");
        for edge in edges {
            assert_eq!(edge["other"], json!("bob"));
            assert_eq!(edge["derived"], json!(true));
            assert_eq!(edge["rule"], json!("sim_emb"));
            assert_eq!(edge["score"], json!(1.0));
            assert!(
                edge["predicate"]
                    .as_str()
                    .unwrap_or("")
                    .contains("vector_similar"),
                "the predicate travels with the edge: {edge}"
            );
        }
    }

    /// Binding: a depth-1 `neighborhood` is the same relationship listing, and
    /// anything deeper is still the traversal table.
    #[test]
    fn test_neighborhood_traverses_one_hop() {
        let db = demo_db();
        let resp = tool_call(
            &db,
            1,
            "neighborhood",
            json!({ "key": "alice", "depth": 1, "json": true }),
        );
        assert!(!is_error(&resp));
        let result = tool_text(&resp);
        assert_eq!(result["key"], "alice");
        assert!(result["types"].as_array().is_some(), "{result}");

        let deep = tool_call(
            &db,
            2,
            "neighborhood",
            json!({ "key": "alice", "depth": 2 }),
        );
        assert!(!is_error(&deep));
        let table = tool_text(&deep);
        assert_eq!(table["columns"], json!(["key", "label", "depth"]));
        assert!(table["rows"].as_array().is_some());
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
        let edges_resp = tool_call(&db, 2, "node_edges", json!({ "key": "x", "json": true }));
        let edges_result = tool_text(&edges_resp);
        let types = edges_result["types"].as_array().expect("types");
        assert!(
            types.iter().any(|t| t["edge_type"] == "SAME_TAG"),
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

    /// `find_similar` with `mask` must exclude hidden node keys from results.
    #[test]
    fn test_find_similar_vector_mask_excludes_hidden() {
        let db = SharedDb::open(&tmp_dir()).expect("open");
        {
            let mut g = db.write();
            // visible: [1,0] — should appear in results.
            g.insert_node(
                "Item",
                "visible",
                vec![(
                    "emb".into(),
                    Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
                )],
            )
            .unwrap();
            // hidden: [1,0] — same direction as query but must not appear.
            g.insert_node(
                "Item",
                "hidden",
                vec![(
                    "emb".into(),
                    Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
                )],
            )
            .unwrap();
        }

        let resp = tool_call(
            &db,
            1,
            "find_similar",
            json!({
                "vector": [1.0, 0.0],
                "field": "emb",
                "label": "Item",
                "k": 10,
                "min": 0.0,
                "mask": ["visible"]
            }),
        );
        assert!(!is_error(&resp), "masked vector search must not error");
        let result = tool_text(&resp);
        let results = result["results"].as_array().expect("results array");

        let keys: Vec<&str> = results.iter().filter_map(|r| r["key"].as_str()).collect();
        assert!(
            keys.contains(&"visible"),
            "visible node must appear in masked results"
        );
        assert!(
            !keys.contains(&"hidden"),
            "hidden node must be excluded by mask"
        );
    }

    /// `find_similar` with `mask` — bad mask value returns a tool error.
    #[test]
    fn test_find_similar_vector_mask_bad_type_is_error() {
        let db = SharedDb::open(&tmp_dir()).expect("open");
        let resp = tool_call(
            &db,
            1,
            "find_similar",
            json!({
                "vector": [1.0, 0.0],
                "field": "emb",
                "k": 5,
                "mask": [42]
            }),
        );
        assert!(
            is_error(&resp),
            "non-string mask element must produce a tool error"
        );
    }

    /// Edge-traversal mode with `mask` must exclude hidden neighbors.
    #[test]
    fn test_find_similar_edge_mask_excludes_hidden_neighbor() {
        let db = SharedDb::open(&tmp_dir()).expect("open");
        {
            let mut g = db.write();
            g.insert_node("P", "alice", vec![]).unwrap();
            g.insert_node("P", "bob", vec![]).unwrap(); // visible
            g.insert_node("P", "carol", vec![]).unwrap(); // hidden
            g.insert_edge("KNOWS", "alice", "bob").unwrap();
            g.insert_edge("KNOWS", "alice", "carol").unwrap();
        }
        // Mask: alice and bob visible; carol hidden.
        let resp = tool_call(
            &db,
            1,
            "find_similar",
            json!({
                "key": "alice",
                "edge_type": "KNOWS",
                "mask": ["alice", "bob"]
            }),
        );
        assert!(!is_error(&resp), "masked edge search must not error");
        let result = tool_text(&resp);
        let similar = result["similar"].as_array().expect("similar array");
        let neighbors: Vec<&str> = similar
            .iter()
            .filter_map(|e| e["neighbor_key"].as_str())
            .collect();
        assert!(neighbors.contains(&"bob"), "bob (visible) must appear");
        assert!(
            !neighbors.contains(&"carol"),
            "carol (hidden) must be excluded"
        );
    }

    /// Edge-traversal mode with `mask`: a hidden query key must not reveal
    /// its existence — response must be a tool error identical to a nonexistent key.
    #[test]
    fn test_find_similar_edge_mask_hidden_key_is_not_found() {
        let db = SharedDb::open(&tmp_dir()).expect("open");
        {
            let mut g = db.write();
            g.insert_node("P", "alice", vec![]).unwrap();
            g.insert_node("P", "bob", vec![]).unwrap();
        }
        // alice exists but is not in the mask — must look like not-found.
        let resp_masked = tool_call(
            &db,
            1,
            "find_similar",
            json!({ "key": "alice", "edge_type": "KNOWS", "mask": ["bob"] }),
        );
        // ghost never exists — use as the reference for "not found".
        let resp_ghost = tool_call(
            &db,
            2,
            "find_similar",
            json!({ "key": "ghost", "edge_type": "KNOWS" }),
        );
        assert!(
            is_error(&resp_masked),
            "hidden query key must produce a tool error"
        );
        assert!(
            is_error(&resp_ghost),
            "nonexistent key must produce a tool error"
        );
        // Both errors must carry the same shape (both are key-not-found).
        assert_eq!(
            tool_err_text(&resp_masked).contains("alice"),
            tool_err_text(&resp_ghost).contains("ghost"),
            "error messages should follow same not-found template"
        );
    }

    /// Binding: `explain_association` now answers in prose, and the report
    /// behind it — what `json: true` returns — is still `explain`'s array,
    /// with one `evidence` object added per relationship and every other
    /// field unchanged.
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
            json!({ "a": "alice", "b": "bob", "json": true }),
        ));
        let explain: Vec<Js> = serde_json::from_value(explain).expect("explain array");
        let mut assoc: Vec<Js> = serde_json::from_value(assoc).expect("assoc array");
        for row in &mut assoc {
            let ev = row
                .as_object_mut()
                .expect("object")
                .remove("evidence")
                .expect("every derived edge carries its evidence");
            assert!(
                ev["similarity"].is_number(),
                "a vector_similar edge reports the cosine it scored: {ev}"
            );
        }
        assert_eq!(explain, assoc, "evidence is the only addition");

        let prose = tool_call(
            &db,
            3,
            "explain_association",
            json!({ "a": "alice", "b": "bob" }),
        );
        let text = prose["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        assert!(
            text.contains("mushroomdb explain — alice ↔ bob:"),
            "the default reply is the digest: {text}"
        );
    }

    // ── history tools ──────────────────────────────────────────────────────────

    /// `edge_history` must return the derived-edge lifecycle (Added event with
    /// rule attribution) and include the `total_commits` horizon field.
    #[test]
    fn test_edge_history_returns_derived_lifecycle_with_rule() {
        let db = demo_db(); // alice+bob + sim_emb rule → SIMILAR derived edge
        let resp = tool_call(&db, 1, "edge_history", json!({ "a": "alice", "b": "bob" }));
        assert!(!is_error(&resp), "edge_history must not error: {resp}");
        let result = tool_text(&resp);

        // Must carry horizon metadata.
        let total = result["total_commits"].as_u64().expect("total_commits");
        assert!(total > 0, "total_commits must be > 0 after ingest + rule");

        // Must have at least one event (the SIMILAR derived-edge addition).
        let events = result["events"].as_array().expect("events array");
        assert!(!events.is_empty(), "expected at least one edge event");

        // At least one event must be Added with a non-null rule (derived edge).
        let derived_added = events
            .iter()
            .any(|ev| ev["event"].as_str() == Some("Added") && !ev["rule"].is_null());
        assert!(
            derived_added,
            "expected a derived Added event with rule attribution: {events:?}"
        );
    }

    /// `was_linked` must return `true` for an edge that was active at the given commit,
    /// and the response must include the echo fields.
    #[test]
    fn test_was_linked_at_valid_commit() {
        let db = SharedDb::open(&tmp_dir()).expect("open");
        {
            let mut g = db.write();
            let opts = IngestOptions {
                key_field: "id".into(),
                auto_fk: AutoFk::Off,
            };
            let rows: Vec<BTreeMap<String, Value>> = vec![
                [("id", Value::Str("x".into()))]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                [("id", Value::Str("y".into()))]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            ];
            g.ingest("N", rows, &opts).expect("ingest");
            g.insert_edge("LINK", "x", "y").expect("edge");
        }
        // There are now at least 2 commits (ingest + edge). Check at the last one.
        let g = db.read();
        let total = g.wal_total_commits().expect("wal_total_commits");
        drop(g);

        let resp = tool_call(
            &db,
            1,
            "was_linked",
            json!({ "a": "x", "b": "y", "edge_type": "LINK", "at_commit": total - 1 }),
        );
        assert!(!is_error(&resp), "was_linked must not error: {resp}");
        let result = tool_text(&resp);
        assert_eq!(result["linked"], true);
        assert_eq!(result["a"], "x");
        assert_eq!(result["edge_type"], "LINK");
    }

    /// `was_linked` with an out-of-horizon commit must return a tool error (not
    /// a protocol error), and the error message must mention the commit range.
    #[test]
    fn test_was_linked_out_of_horizon_returns_tool_error() {
        let db = SharedDb::open(&tmp_dir()).expect("open");
        {
            let mut g = db.write();
            g.insert_node("N", "a", vec![]).expect("node a");
            g.insert_node("N", "b", vec![]).expect("node b");
        }
        // Commit 999 is well beyond the WAL.
        let resp = tool_call(
            &db,
            1,
            "was_linked",
            json!({ "a": "a", "b": "b", "edge_type": "X", "at_commit": 999 }),
        );
        // isError true = tool-level error (not a JSON-RPC protocol error).
        assert!(
            is_error(&resp),
            "out-of-range commit must be a tool error: {resp}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        assert!(
            text.contains("out of range") || text.contains("range"),
            "error must mention range: {text}"
        );
    }

    /// `node_history` tool must return the node's WAL history and the
    /// `total_commits` horizon field.
    #[test]
    fn test_node_history_via_mcp() {
        let db = demo_db(); // alice + bob, with a SIMILAR rule
        let resp = tool_call(&db, 1, "node_history", json!({ "key": "alice" }));
        assert!(!is_error(&resp), "node_history must not error: {resp}");
        let result = tool_text(&resp);

        assert_eq!(result["key"], "alice");
        let total = result["total_commits"].as_u64().expect("total_commits");
        assert!(total > 0, "total_commits must be > 0");

        let history = result["history"].as_array().expect("history array");
        assert!(
            !history.is_empty(),
            "alice should have at least one history entry"
        );

        // First event should be a NodeInserted.
        let first_change = &history[0]["change"];
        assert_eq!(first_change["type"], "NodeInserted");
        assert_eq!(first_change["label"], "Person");
    }
}
