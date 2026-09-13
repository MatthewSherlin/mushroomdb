//! Binding tests for the MCP JSON-RPC 2.0 stdio loop.
//!
//! Framing is newline-delimited JSON only (`Cursor<Vec<u8>>` transcripts).
//! LSP `Content-Length` framing is not accepted.
//!
//! # Error split (protocol vs tool)
//!
//! Protocol errors are JSON-RPC `error` objects:
//! - `-32700` — unparseable line (invalid JSON / invalid UTF-8)
//! - `-32600` — parsed JSON that is not a request object, or a request
//!   with missing / non-string `method`
//! - `-32601` — unknown `method` on a request
//! - `-32602` — `tools/call` *envelope* invalid: `params` not an object,
//!   missing / non-string `name`, `arguments` present but not an object,
//!   or unknown tool name
//!
//! Tool-level failures are JSON-RPC *results* with `isError: true` and a
//! text message: missing or wrong-typed fields inside a known tool's
//! `arguments`, and every `GraphError` from core-api (bad Cypher, ingest
//! shape, unknown key, …).

use core_api::repograph::UNTRUSTED_FRAMING;
use core_api::{SharedDb, Value};
use serde_json::{json, Value as Js};
use server::run_mcp_stdio;
use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-mcp-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn open(name: &str) -> SharedDb {
    SharedDb::open(&tmp(name)).unwrap()
}

fn exchange(db: SharedDb, stdin: &str) -> (std::io::Result<()>, Vec<u8>) {
    exchange_at(db, None, stdin)
}

/// Like [`exchange`], but tells the loop where the store is on disk — what
/// `mushroomdb mcp <db>` passes and what the `sync` tool needs.
fn exchange_at(
    db: SharedDb,
    db_dir: Option<PathBuf>,
    stdin: &str,
) -> (std::io::Result<()>, Vec<u8>) {
    let mut reader = Cursor::new(stdin.as_bytes().to_vec());
    let mut writer = Cursor::new(Vec::new());
    let res = run_mcp_stdio(db, db_dir, &mut reader, &mut writer);
    (res, writer.into_inner())
}

/// Like [`exchange`], but with the full tool list — what `mushroomdb mcp
/// --all-tools` runs.
fn exchange_all_tools(db: SharedDb, stdin: &str) -> (std::io::Result<()>, Vec<u8>) {
    let mut reader = Cursor::new(stdin.as_bytes().to_vec());
    let mut writer = Cursor::new(Vec::new());
    let res = server::run_mcp_stdio_with(db, None, true, &mut reader, &mut writer);
    (res, writer.into_inner())
}

fn parse_lines(out: &[u8]) -> Vec<Js> {
    let text = String::from_utf8(out.to_vec()).expect("stdout utf-8");
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("stdout line json: {e}: {l}")))
        .collect()
}

fn line(obj: Js) -> String {
    format!("{obj}\n")
}

fn req(id: Js, method: &str, params: Option<Js>) -> String {
    let mut obj = json!({"jsonrpc": "2.0", "id": id, "method": method});
    if let Some(p) = params {
        obj["params"] = p;
    }
    line(obj)
}

fn notify(method: &str, params: Option<Js>) -> String {
    let mut obj = json!({"jsonrpc": "2.0", "method": method});
    if let Some(p) = params {
        obj["params"] = p;
    }
    line(obj)
}

fn call(id: i64, name: &str, arguments: Js) -> String {
    req(
        json!(id),
        "tools/call",
        Some(json!({"name": name, "arguments": arguments})),
    )
}

fn content_json(reply: &Js) -> Js {
    assert!(
        reply.get("error").is_none() || reply["error"].is_null(),
        "expected tool result, got protocol error: {reply}"
    );
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("content[0].text string: {reply}"));
    assert_eq!(reply["result"]["content"][0]["type"], "text");
    serde_json::from_str(text).unwrap_or_else(|e| panic!("tool text json: {e}: {text}"))
}

fn seed_person(db: &SharedDb, key: &str) {
    db.write()
        .insert_node("Person", key, vec![("id".into(), Value::Str(key.into()))])
        .unwrap();
}

/// Binding: initialize result fields are exact; `notifications/initialized` is silent.
#[test]
fn handshake_initialize_then_initialized_is_silent() {
    let stdin = format!(
        "{}{}",
        req(
            json!(1),
            "initialize",
            Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            })),
        ),
        notify("notifications/initialized", None),
    );
    let (res, out) = exchange(open("handshake"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(
        replies.len(),
        1,
        "initialized must not emit a response: {replies:?}"
    );
    let r = &replies[0];
    assert_eq!(r["jsonrpc"], "2.0");
    assert_eq!(r["id"], 1);
    assert!(r.get("error").is_none() || r["error"].is_null());
    assert_eq!(
        r["result"],
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mushroomdb", "version": env!("CARGO_PKG_VERSION")}
        })
    );
}

/// Binding: `--all-tools` returns all expected tools with the specified
/// schemas.
#[test]
fn tools_list_returns_all_tools_with_schemas() {
    let stdin = req(json!(1), "tools/list", None);
    let (res, out) = exchange_all_tools(open("list"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);
    let tools = replies[0]["result"]["tools"]
        .as_array()
        .expect("result.tools array");
    let names: BTreeSet<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    // Original eight tools plus agent-memory tools plus history tools.
    for expected in &[
        "query",
        "ingest_json",
        "explain",
        "stats",
        "neighborhood",
        "node_info",
        "node_edges",
        "create_rule",
        "upsert_entity",
        "find_similar",
        "explain_association",
        "hybrid_search",
        "node_history",
        "edge_history",
        "was_linked",
        "rename_node",
    ] {
        assert!(names.contains(*expected), "missing tool: {expected}");
    }
    assert_eq!(tools.len(), 27);

    let by_name = |n: &str| {
        tools
            .iter()
            .find(|t| t["name"] == n)
            .unwrap_or_else(|| panic!("missing tool {n}"))
    };

    let query = by_name("query");
    assert_eq!(query["inputSchema"]["type"], "object");
    assert!(query["inputSchema"]["properties"].get("cypher").is_some());
    assert!(query["inputSchema"]["properties"].get("params").is_some());
    // A host defers the schema, so `as_of` has to be findable in it.
    assert_eq!(
        query["inputSchema"]["properties"]["as_of"]["type"],
        json!("integer")
    );
    assert_eq!(query["inputSchema"]["required"], json!(["cypher"]));

    let ingest = by_name("ingest_json");
    assert!(ingest["inputSchema"]["properties"].get("label").is_some());
    assert!(ingest["inputSchema"]["properties"]
        .get("rows_json")
        .is_some());
    assert!(ingest["inputSchema"]["properties"]
        .get("key_field")
        .is_some());
    assert!(ingest["inputSchema"]["properties"]
        .get("auto_fk_suffix")
        .is_some());
    assert_eq!(
        ingest["inputSchema"]["required"],
        json!(["label", "rows_json"])
    );

    let explain = by_name("explain");
    assert_eq!(
        explain["inputSchema"]["properties"]["a"],
        json!({"type": "string", "minLength": 1})
    );
    assert_eq!(
        explain["inputSchema"]["properties"]["b"],
        json!({"type": "string", "minLength": 1})
    );
    assert_eq!(explain["inputSchema"]["required"], json!(["a", "b"]));

    let stats = by_name("stats");
    assert_eq!(stats["inputSchema"]["type"], "object");

    let nb = by_name("neighborhood");
    assert!(nb["inputSchema"]["properties"].get("key").is_some());
    assert!(nb["inputSchema"]["properties"].get("depth").is_some());
    assert!(nb["inputSchema"]["properties"].get("edge_types").is_some());
    assert!(nb["inputSchema"]["properties"].get("direction").is_some());
    assert_eq!(nb["inputSchema"]["required"], json!(["key"]));

    let info = by_name("node_info");
    assert_eq!(info["inputSchema"]["type"], "object");
    assert!(info["inputSchema"]["properties"].get("key").is_some());
    assert_eq!(info["inputSchema"]["required"], json!(["key"]));

    let edges = by_name("node_edges");
    assert_eq!(edges["inputSchema"]["type"], "object");
    assert!(edges["inputSchema"]["properties"].get("key").is_some());
    assert!(edges["inputSchema"]["properties"]
        .get("edge_type")
        .is_some());
    assert!(edges["inputSchema"]["properties"].get("limit").is_some());
    assert_eq!(edges["inputSchema"]["required"], json!(["key"]));

    let at = by_name("edges_at");
    assert_eq!(at["inputSchema"]["required"], json!(["key", "at"]));

    let what_if = by_name("what_if");
    assert_eq!(
        what_if["inputSchema"]["required"],
        json!(["key", "field", "value"])
    );

    let cr = by_name("create_rule");
    assert_eq!(cr["inputSchema"]["type"], "object");
    assert!(cr["inputSchema"]["properties"].get("predicate").is_some());
    assert_eq!(
        cr["inputSchema"]["required"],
        json!(["name", "src_label", "dst_label", "predicate", "edge_type"])
    );
    assert!(ingest["inputSchema"]["properties"].get("edges").is_some());
}

/// Binding: tools/call happy path for each of the five tools against a seeded db.
#[test]
fn tools_call_happy_path_for_each_tool() {
    let db = open("each-tool");
    {
        let mut w = db.write();
        w.insert_node("Org", "acme", vec![]).unwrap();
        w.insert_node(
            "Person",
            "p1",
            vec![
                ("id".into(), Value::Str("p1".into())),
                ("org_id".into(), Value::Str("acme".into())),
            ],
        )
        .unwrap();
        w.insert_node("Person", "p2", vec![("id".into(), Value::Str("p2".into()))])
            .unwrap();
        w.insert_edge("KNOWS", "p1", "p2").unwrap();
        w.create_rule(core_api::RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: core_api::Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
    }

    let stdin = format!(
        "{}{}{}{}{}",
        call(
            1,
            "query",
            json!({"cypher": "MATCH (t:Person {id: $tid}) RETURN t", "params": {"tid": "p1"}}),
        ),
        call(
            2,
            "ingest_json",
            json!({
                "label": "Person",
                "rows_json": "[{\"id\":\"p3\",\"name\":\"ada\"}]"
            }),
        ),
        call(3, "explain", json!({"a": "p1", "b": "acme"})),
        call(4, "stats", json!({})),
        call(
            5,
            "neighborhood",
            json!({
                "key": "p1",
                "depth": 2,
                "edge_types": ["KNOWS"],
                "direction": "out"
            }),
        ),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 5);

    let q = content_json(&replies[0]);
    assert_eq!(q["columns"], json!(["t"]));
    assert_eq!(q["rows"], json!([["p1"]]));

    let ingest = content_json(&replies[1]);
    assert_eq!(ingest["inserted"], json!(1));
    assert_eq!(ingest["row_errors"], json!([]));
    assert!(db.read().has_node("p3"));

    let expl = content_json(&replies[2]);
    assert!(expl.is_array());
    assert_eq!(expl[0]["rule"], json!("works_at"));
    assert_eq!(expl[0]["edge_type"], json!("WORKS_AT"));
    assert_eq!(expl[0]["src_key"], json!("p1"));
    assert_eq!(expl[0]["dst_key"], json!("acme"));
    assert_eq!(expl[0]["predicate"]["kind"], json!("key_match"));
    assert_eq!(expl[0]["predicate"]["fields"], json!(["org_id"]));

    let stats = content_json(&replies[3]);
    assert!(stats["nodes_live"].as_u64().unwrap() >= 3);

    let nb = content_json(&replies[4]);
    assert_eq!(nb["columns"], json!(["key", "label", "depth"]));
    assert_eq!(nb["rows"], json!([["p2", "Person", 1]]));
}

/// Binding: MCP `query` dispatches CREATE through the write lock.
#[test]
fn query_create_is_a_write() {
    let db = open("mcp-create");
    let stdin = call(
        1,
        "query",
        json!({"cypher": "CREATE (n:L {id: 'k'}) RETURN n"}),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);
    assert!(
        replies[0].get("error").is_none() || replies[0]["error"].is_null(),
        "CREATE via query must not be a protocol error: {}",
        replies[0]
    );
    assert_ne!(
        replies[0]["result"]["isError"],
        json!(true),
        "CREATE via query must succeed: {}",
        replies[0]
    );
    let q = content_json(&replies[0]);
    assert_eq!(q["columns"], json!(["n"]));
    assert_eq!(q["rows"], json!([["k"]]));
    assert_eq!(db.read().stats().nodes_live, 1);
}

/// Binding: `node_info` still answers in the HTTP wire shape, and `node_edges`
/// answers with the grouped listing — every edge under its type, the derived
/// one carrying the rule that wrote it.
#[test]
fn node_info_and_edges_tool_parity() {
    let db = open("node-tools");
    {
        let mut w = db.write();
        w.insert_node("Org", "acme", vec![]).unwrap();
        w.create_rule(core_api::RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: core_api::Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
        w.insert_node(
            "Person",
            "p1",
            vec![
                ("id".into(), Value::Str("p1".into())),
                ("org_id".into(), Value::Str("acme".into())),
            ],
        )
        .unwrap();
        w.insert_node("Person", "p2", vec![]).unwrap();
        w.insert_edge("KNOWS", "p1", "p2").unwrap();
    }

    let stdin = format!(
        "{}{}{}",
        call(1, "node_info", json!({"key": "p1"})),
        call(2, "node_edges", json!({"key": "p1", "json": true})),
        call(3, "node_info", json!({"key": "ghost"})),
    );
    let (res, out) = exchange(db, &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 3);

    let info = content_json(&replies[0]);
    assert_eq!(info["key"], json!("p1"));
    assert_eq!(info["label"], json!("Person"));
    assert_eq!(info["props"]["id"], json!("p1"));
    assert_eq!(info["props"]["org_id"], json!("acme"));

    let edges = content_json(&replies[1]);
    assert_eq!(
        edges,
        json!({
            "key": "p1",
            "total": 2,
            "listed": 2,
            "types": [
                {
                    "edge_type": "KNOWS",
                    "count": 1,
                    "listed": 1,
                    "edges": [{
                        "edge_type": "KNOWS",
                        "other": "p2",
                        "direction": "out",
                        "derived": false,
                        "rule": null,
                        "score": null,
                        "predicate": null
                    }]
                },
                {
                    "edge_type": "WORKS_AT",
                    "count": 1,
                    "listed": 1,
                    "edges": [{
                        "edge_type": "WORKS_AT",
                        "other": "acme",
                        "direction": "out",
                        "derived": true,
                        "rule": "works_at",
                        "score": 1.0,
                        "predicate": "key_match on org_id"
                    }]
                }
            ]
        })
    );

    assert_eq!(replies[2]["result"]["isError"], json!(true));
    let msg = replies[2]["result"]["content"][0]["text"]
        .as_str()
        .expect("error text");
    assert_eq!(msg, "node key not found: ghost");
}

/// Marquee: ingest through MCP → query through MCP → explain shows auto-FK provenance.
#[test]
fn agent_memory_loop() {
    let db = open("agent-loop");
    let stdin = format!(
        "{}{}{}{}{}{}",
        req(
            json!(1),
            "initialize",
            Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "agent", "version": "0"}
            })),
        ),
        notify("notifications/initialized", None),
        call(
            2,
            "ingest_json",
            json!({
                "label": "Org",
                "rows_json": "[{\"id\":\"acme\",\"name\":\"Acme\"}]"
            }),
        ),
        call(
            3,
            "ingest_json",
            json!({
                "label": "Person",
                "rows_json": "[{\"id\":\"p1\",\"org_id\":\"acme\",\"name\":\"ada\"}]"
            }),
        ),
        call(
            4,
            "query",
            json!({
                "cypher": "MATCH (t:Person {id: $tid}) RETURN t",
                "params": {"tid": "p1"}
            }),
        ),
        call(5, "explain", json!({"a": "p1", "b": "acme"})),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(
        replies.len(),
        5,
        "initialize + 2 ingest + query + explain; initialized silent"
    );

    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "mushroomdb");
    assert_eq!(
        replies[0]["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    let org = content_json(&replies[1]);
    assert_eq!(org["inserted"], json!(1));

    let person = content_json(&replies[2]);
    assert_eq!(person["inserted"], json!(1));
    assert_eq!(person["rules_created"], json!(["auto_fk_person_org_id"]));
    assert!(db.read().has_node("p1"));

    let q = content_json(&replies[3]);
    assert_eq!(q["columns"], json!(["t"]));
    assert_eq!(q["rows"], json!([["p1"]]));

    let expl = content_json(&replies[4]);
    assert_eq!(expl[0]["rule"], json!("auto_fk_person_org_id"));
    assert_eq!(expl[0]["edge_type"], json!("ORG"));
    assert_eq!(expl[0]["src_key"], json!("p1"));
    assert_eq!(expl[0]["dst_key"], json!("acme"));
    // auto-FK stores no weight prop; explain recomputes the KeyMatch score.
    assert_eq!(expl[0]["weight"], json!(1.0));
    assert_eq!(expl[0]["predicate"]["kind"], json!("key_match"));
    assert_eq!(expl[0]["predicate"]["fields"], json!(["org_id"]));
}

/// Binding: a malformed line is -32700; the next valid request still works.
#[test]
fn malformed_line_is_parse_error_then_next_request_works() {
    let stdin = format!("this is not json\n{}", req(json!(1), "tools/list", None));
    let (res, out) = exchange(open("malformed"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["jsonrpc"], "2.0");
    assert_eq!(replies[0]["id"], Js::Null);
    assert_eq!(replies[0]["error"]["code"], json!(-32700));
    assert!(replies[1]["result"]["tools"].is_array());
}

/// Binding: unknown method on a request is -32601.
#[test]
fn unknown_method_is_minus_32601() {
    let stdin = req(json!("abc"), "no/such", None);
    let (res, out) = exchange(open("unknown"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["id"], "abc");
    assert_eq!(replies[0]["error"]["code"], json!(-32601));
}

/// Binding: envelope failures are -32602; tool-argument / execution failures are isError.
#[test]
fn bad_tool_args_split_protocol_vs_tool() {
    let db = open("bad-args");
    seed_person(&db, "p1");
    let stdin = format!(
        "{}{}{}{}{}{}",
        req(
            json!(1),
            "tools/call",
            Some(json!({"arguments": {"cypher": "RETURN 1"}})),
        ),
        call(2, "nope", json!({})),
        req(json!(5), "tools/call", Some(json!(42))),
        req(
            json!(6),
            "tools/call",
            Some(json!({"name": "query", "arguments": "string"})),
        ),
        call(3, "query", json!({})),
        call(4, "query", json!({"cypher": "MATCH (n)"})),
    );
    let (res, out) = exchange(db, &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 6);

    // Protocol: tools/call missing name.
    assert_eq!(replies[0]["error"]["code"], json!(-32602));
    // Protocol: unknown tool name.
    assert_eq!(replies[1]["error"]["code"], json!(-32602));
    // Protocol: params is not an object.
    assert_eq!(replies[2]["id"], 5);
    assert_eq!(replies[2]["error"]["code"], json!(-32602));
    // Protocol: arguments is not an object.
    assert_eq!(replies[3]["id"], 6);
    assert_eq!(replies[3]["error"]["code"], json!(-32602));
    // Tool-level: known tool, missing required argument.
    assert_eq!(replies[4]["result"]["isError"], json!(true));
    assert_eq!(replies[4]["result"]["content"][0]["type"], "text");
    assert!(replies[4]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("cypher"));
    // Tool-level: GraphError (bad Cypher) is not a protocol error.
    assert!(replies[5].get("error").is_none() || replies[5]["error"].is_null());
    assert_eq!(replies[5]["result"]["isError"], json!(true));
    let msg = replies[5]["result"]["content"][0]["text"]
        .as_str()
        .expect("error text");
    assert!(msg.contains("parse:"), "expected parse: detail, got {msg}");
}

/// Binding: a parsed non-object line is -32600 Invalid Request, not parse error.
#[test]
fn non_object_json_is_invalid_request() {
    let stdin = format!(
        "[1]\n42\n\"hi\"\ntrue\n{}",
        req(json!(1), "tools/list", None)
    );
    let (res, out) = exchange(open("non-object"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 5);
    for r in &replies[..4] {
        assert_eq!(r["id"], Js::Null);
        assert_eq!(r["error"]["code"], json!(-32600));
        assert_eq!(r["error"]["message"], "Invalid Request");
    }
    assert!(replies[4]["result"]["tools"].is_array());
}

/// Binding: missing or non-string method on a request is -32600, not -32601.
#[test]
fn missing_or_non_string_method_is_invalid_request() {
    let stdin = format!(
        "{}\n{}\n",
        json!({"jsonrpc": "2.0", "id": 1}),
        json!({"jsonrpc": "2.0", "id": 2, "method": 42}),
    );
    let (res, out) = exchange(open("no-method"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[0]["error"]["code"], json!(-32600));
    assert_eq!(replies[0]["error"]["message"], "Invalid Request");
    assert_eq!(replies[1]["id"], 2);
    assert_eq!(replies[1]["error"]["code"], json!(-32600));
    assert_eq!(replies[1]["error"]["message"], "Invalid Request");
}

/// Binding: `id: null` is a request; the response echoes `id: null`.
#[test]
fn id_null_request_echoes_null() {
    let stdin = req(Js::Null, "tools/list", None);
    let (res, out) = exchange(open("id-null"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["id"], Js::Null);
    assert!(replies[0]["result"]["tools"].is_array());
}

/// Binding: a notification (no `id`) never writes a response.
#[test]
fn notification_without_id_emits_no_bytes() {
    let stdin = notify("tools/list", None);
    let (res, out) = exchange(open("notify"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    assert!(out.is_empty(), "notification must not write: {out:?}");
}

/// Binding: EOF on the reader is a clean `Ok(())`.
#[test]
fn eof_exits_ok() {
    let mut reader = Cursor::new(Vec::<u8>::new());
    let mut writer = Cursor::new(Vec::new());
    let res = run_mcp_stdio(open("eof"), None, &mut reader, &mut writer);
    assert!(res.is_ok(), "{res:?}");
    assert!(writer.into_inner().is_empty());
}

/// Binding: ingest_json edges + create_rule match the HTTP write surface.
#[test]
fn ingest_edges_and_create_rule_tools() {
    let db = open("write-tools");
    {
        let mut w = db.write();
        w.insert_node("Person", "a", vec![]).unwrap();
        w.insert_node("Person", "b", vec![]).unwrap();
        w.insert_node("Org", "o1", vec![("founded_year".into(), Value::Int(2010))])
            .unwrap();
        w.insert_node("Org", "o2", vec![("founded_year".into(), Value::Int(2011))])
            .unwrap();
    }
    let stdin = format!(
        "{}{}",
        call(
            1,
            "ingest_json",
            json!({
                "label": "Person",
                "rows_json": "[]",
                "edges": [{"edge_type": "KNOWS", "src": "a", "dst": "b"}]
            }),
        ),
        call(
            2,
            "create_rule",
            json!({
                "name": "founded_within",
                "src_label": "Org",
                "dst_label": "Org",
                "predicate": {"NumericWithin": {"field": "founded_year", "tolerance": 2.0}},
                "edge_type": "FOUNDED_WITHIN",
                "weight_prop": "score",
                "max_edges": null
            }),
        ),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 2);
    let ingest = content_json(&replies[0]);
    assert_eq!(ingest["edges_inserted"], json!(1));
    let created = content_json(&replies[1]);
    assert_eq!(created, json!({"ok": true, "name": "founded_within"}));
    let edges = db.read().node_edges("a").unwrap();
    assert!(edges.iter().any(|e| e.edge_type == "KNOWS" && !e.derived));
    assert!(db.read().rules().iter().any(|r| r.name == "founded_within"));
}

/// Binding: create_rule without `weight_prop` stores the score under `weight`,
/// so a derived edge's score is queryable instead of null.
#[test]
fn mcp_create_rule_defaults_weight_prop_to_weight() {
    let db = open("default-weight-prop");
    {
        let mut w = db.write();
        for key in ["p1", "p2"] {
            w.insert_node(
                "Person",
                key,
                vec![("industry".into(), Value::Str("design".into()))],
            )
            .unwrap();
        }
        for key in ["o1", "o2"] {
            w.insert_node(
                "Org",
                key,
                vec![("industry".into(), Value::Str("design".into()))],
            )
            .unwrap();
        }
    }
    let stdin = format!(
        "{}{}",
        call(
            1,
            "create_rule",
            json!({
                "name": "same",
                "src_label": "Person",
                "dst_label": "Org",
                "predicate": {"FieldEqual": {"field": "industry"}},
                "edge_type": "SAME"
            }),
        ),
        call(
            2,
            "query",
            json!({"cypher": "MATCH (p:Person)-[r:SAME]->(o:Org) RETURN r.weight LIMIT 1"}),
        ),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 2);
    assert_eq!(
        content_json(&replies[0]),
        json!({"ok": true, "name": "same"})
    );
    let q = content_json(&replies[1]);
    assert_eq!(q["columns"], json!(["r.weight"]));
    assert_eq!(
        q["rows"][0][0],
        json!(1.0),
        "score must be stored under the default weight prop, got {q}"
    );
}

/// Binding: ingest_json mixed batch is atomic — a bad edge persists no nodes.
#[test]
fn ingest_json_bad_edge_is_atomic() {
    let db = open("mcp-atomic");
    let stdin = call(
        1,
        "ingest_json",
        json!({
            "label": "Person",
            "rows_json": "[{\"id\":\"newbie\"}]",
            "edges": [{"edge_type": "KNOWS", "src": "newbie", "dst": "ghost"}]
        }),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["result"]["isError"], json!(true));
    let msg = replies[0]["result"]["content"][0]["text"]
        .as_str()
        .expect("error text");
    assert!(
        msg.contains("node key not found"),
        "preview error, got {msg}"
    );
    assert!(
        !db.read().has_node("newbie"),
        "newbie must not persist after a rejected mixed batch"
    );
}

// ---------------------------------------------------------------------------
// Task 3: hybrid_search MCP tool
// ---------------------------------------------------------------------------

/// hybrid_search returns fused results with both a text and vector match.
/// Fixture: three "Doc" nodes:
///   "both"   — body="hello", emb=[1,0] close to query [1,0].
///   "t_only" — body="hello", emb=[-1,0] (antipodal → cosine -1 < 0 → filtered).
///   "v_only" — body="other", emb=[1,0] exactly aligned with query.
///
/// Text ranking: both(rank 1), t_only(rank 2).
/// Vector ranking: both(rank 1, cosine 1.0), v_only filtered? No wait —
/// actually both=[1,0] and v_only=[1,0] tie at cosine 1.0.
/// Use distinct vectors: both=[0.9, 0.436] (≈ 0.9 cosine) and v_only=[1,0].
///
/// Text: both rank 1, t_only rank 2.
/// Vector: v_only rank 1, both rank 2.
/// RRF: both = 1/61+1/62, v_only = 1/61, t_only = 1/62.
/// Order: both > v_only > t_only.
#[test]
fn hybrid_search_round_trip() {
    let db = open("mcp-hybrid");

    // Enable fulltext.
    {
        let mut w = db.write();
        w.enable_fulltext("Doc", "body").unwrap();
        w.insert_node(
            "Doc",
            "both",
            vec![
                ("body".into(), Value::Str("hello".into())),
                (
                    "emb".into(),
                    Value::List(vec![Value::Float(1.0), Value::Float(0.5)]),
                ),
            ],
        )
        .unwrap();
        w.insert_node(
            "Doc",
            "t_only",
            vec![
                ("body".into(), Value::Str("hello".into())),
                (
                    "emb".into(),
                    Value::List(vec![Value::Float(-1.0), Value::Float(0.0)]),
                ),
            ],
        )
        .unwrap();
        w.insert_node(
            "Doc",
            "v_only",
            vec![
                ("body".into(), Value::Str("other".into())),
                (
                    "emb".into(),
                    Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
                ),
            ],
        )
        .unwrap();
    }

    let stdin = call(
        1,
        "hybrid_search",
        json!({
            "query_text": "hello",
            "text_field": "body",
            "vector": [1.0, 0.0],
            "vector_field": "emb",
            "label": "Doc",
            "k": 3
        }),
    );
    let (res, out) = exchange(db, &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);

    let payload = content_json(&replies[0]);
    assert!(!replies[0]["result"]["isError"].as_bool().unwrap_or(false));

    let results = payload["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3, "all three nodes returned");

    // "both" must be first with the highest fused score.
    assert_eq!(results[0]["key"], json!("both"), "both must rank first");
    assert_eq!(
        results[1]["key"],
        json!("v_only"),
        "v_only must rank second"
    );
    assert_eq!(results[2]["key"], json!("t_only"), "t_only must rank third");

    let s_both = results[0]["score"].as_f64().expect("score float");
    let s_v_only = results[1]["score"].as_f64().expect("score float");
    let s_t_only = results[2]["score"].as_f64().expect("score float");
    let expected_both = 1.0_f64 / 61.0 + 1.0_f64 / 62.0;
    let expected_v_only = 1.0_f64 / 61.0;
    let expected_t_only = 1.0_f64 / 62.0;
    assert!((s_both - expected_both).abs() < 1e-12, "both score");
    assert!((s_v_only - expected_v_only).abs() < 1e-12, "v_only score");
    assert!((s_t_only - expected_t_only).abs() < 1e-12, "t_only score");
}

/// hybrid_search with no vector → text-only ranking; missing required fields → error.
#[test]
fn hybrid_search_text_only_and_missing_field_errors() {
    let db = open("mcp-hybrid-errs");

    {
        let mut w = db.write();
        w.enable_fulltext("Doc", "body").unwrap();
        w.insert_node(
            "Doc",
            "alpha",
            vec![("body".into(), Value::Str("foo".into()))],
        )
        .unwrap();
    }

    // Text-only (no vector field).
    let stdin = call(
        1,
        "hybrid_search",
        json!({ "query_text": "foo", "text_field": "body", "k": 5 }),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let payload = content_json(&replies[0]);
    let results = payload["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["key"], json!("alpha"));

    // Missing query_text → tool error.
    let stdin2 = call(2, "hybrid_search", json!({ "text_field": "body" }));
    let (_, out2) = exchange(db, &stdin2);
    let replies2 = parse_lines(&out2);
    assert_eq!(
        replies2[0]["result"]["isError"],
        json!(true),
        "missing query_text must be an error"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Task tools: explore, map, context, impact, owners, why, recall, remember,
// sync
//
// These nine answer a question about a graphed repository rather than about
// the graph API, so they come first in `tools/list` and the graph tools listed
// beside them are prefixed `Advanced:`. Each returns the rendered digest as
// its text content and nothing else; a caller that wants the report passes
// `json: true` and gets it *as* the text.
// ─────────────────────────────────────────────────────────────────────────────

/// The ten task tools, in the order `tools/list` must list them.
const TASK_TOOLS: [&str; 14] = [
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
];

/// The thirteen graph tools, in their established order, after the task tools.
const ADVANCED_TOOLS: [&str; 13] = [
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
];

/// The three files of the synthetic code store, and who tops each.
const CODE_FILES: [(&str, &str); 3] = [
    ("src/core.rs", "a@example.test"),
    ("src/util.rs", "a@example.test"),
    ("src/web.rs", "b@example.test"),
];
/// Commits in the synthetic code store.
const CODE_COMMITS: usize = 4;
/// Unix seconds of its oldest commit. Fixed, so nothing here reads a clock.
const CODE_T0: i64 = 1_700_000_000;

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

fn list(items: &[String]) -> Value {
    Value::List(items.iter().map(|i| s(i)).collect())
}

fn code_sha(i: usize) -> String {
    format!("{:07x}{:033x}", 0x00ab_cd00usize + i * 4093, i)
}

/// Files commit `i` touched: every commit touches `core` and `web`, and the
/// first also touches `util` — so `core`/`web` overlap fully and `core`/`util`
/// overlap at exactly the `co_changed` threshold.
fn code_touched(i: usize) -> Vec<&'static str> {
    if i == 0 {
        vec!["src/core.rs", "src/util.rs", "src/web.rs"]
    } else {
        vec!["src/core.rs", "src/web.rs"]
    }
}

/// The rules `ingest-git` declares, reduced to the ones these tools read.
///
/// Recreated here rather than imported: core-api's fixture is a private module
/// of its own test crates, and the server crate cannot reach into it.
fn code_rules() -> Vec<core_api::RuleDef> {
    fn key_rule(name: &str, src: &str, dst: &str, field: &str, edge: &str) -> core_api::RuleDef {
        let predicate = core_api::Predicate::KeyMatch {
            field: field.into(),
        };
        let max_edges = Some(core_api::default_max_edges(&predicate));
        core_api::RuleDef {
            name: name.into(),
            src_label: src.into(),
            dst_label: dst.into(),
            predicate,
            edge_type: edge.into(),
            weight_prop: None,
            max_edges,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }
    let mut out = vec![
        key_rule(
            "auto_fk_symbol_file_id",
            "Symbol",
            "File",
            "file_id",
            "DEFINES",
        ),
        key_rule("imports", "File", "File", "imports", "IMPORTS"),
        key_rule("calls", "Symbol", "Symbol", "calls_to", "CALLS"),
        key_rule(
            "auto_fk_commit_author_id",
            "Commit",
            "Author",
            "author_id",
            "AUTHOR",
        ),
        key_rule(
            "auto_fk_file_top_author_id",
            "File",
            "Author",
            "top_author_id",
            "TOP_AUTHOR",
        ),
    ];
    for label in core_api::repograph::rules::ABOUT_LABELS {
        out.push(core_api::repograph::rules::about_rule(label));
    }
    let co = core_api::Predicate::Overlap {
        field: "commits".into(),
        min: 0.25,
    };
    out.push(core_api::RuleDef {
        name: "co_changed".into(),
        src_label: "File".into(),
        dst_label: "File".into(),
        predicate: co.clone(),
        edge_type: "CO_CHANGED".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(10),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    });
    out.push(core_api::RuleDef {
        name: "knows".into(),
        src_label: "Author".into(),
        dst_label: "File".into(),
        predicate: co,
        edge_type: "KNOWS".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(20),
        approximate: false,
        via_label: Some("File".into()),
        via_edge: Some("TOP_AUTHOR".into()),
        via_dir: Some(core_api::Direction::In),
    });
    out
}

/// Write the synthetic code graph into an already-open store.
fn seed_code_graph(db: &SharedDb) {
    let mut w = db.write();
    for (key, name) in [
        ("a@example.test", "Ada Example"),
        ("b@example.test", "Bea Example"),
    ] {
        w.insert_node("Author", key, vec![("name".into(), s(name))])
            .expect("author");
    }

    let mut commits_of: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    let mut authors_of: std::collections::BTreeMap<&str, std::collections::BTreeMap<&str, usize>> =
        Default::default();
    for i in 0..CODE_COMMITS {
        let author = if i % 2 == 0 {
            "a@example.test"
        } else {
            "b@example.test"
        };
        for f in code_touched(i) {
            commits_of.entry(f).or_default().push(code_sha(i));
            *authors_of.entry(f).or_default().entry(author).or_default() += 1;
        }
    }

    for (key, top) in CODE_FILES {
        let commits = commits_of.get(key).cloned().unwrap_or_default();
        let counts: Vec<String> = authors_of
            .get(key)
            .into_iter()
            .flatten()
            .map(|(email, n)| format!("{email}\t{n}"))
            .collect();
        let mut props = vec![
            ("id".into(), s(key)),
            ("path".into(), s(key)),
            ("dir".into(), s("src")),
            ("ext".into(), s("rs")),
            ("lang".into(), s("rust")),
            ("lines".into(), Value::Int(42)),
            ("top_author_id".into(), s(top)),
            ("n_commits".into(), Value::Int(commits.len() as i64)),
            ("commits".into(), list(&commits)),
            ("author_counts".into(), list(&counts)),
        ];
        // Both other files import the core one, and quote the line they did so on.
        if key != "src/core.rs" {
            props.push(("imports".into(), list(&["src/core.rs".to_string()])));
            props.push(("import_lines".into(), list(&["src/core.rs\t3".to_string()])));
        }
        w.insert_node("File", key, props).expect("file");
    }

    for (file, name, callee) in [
        ("src/core.rs", "core::init", None),
        ("src/web.rs", "web::serve", Some("src/core.rs#core::init")),
    ] {
        let key = format!("{file}#{name}");
        let mut props = vec![
            ("id".into(), s(&key)),
            ("name".into(), s(name)),
            ("kind".into(), s("function")),
            ("path".into(), s(file)),
            ("file_id".into(), s(file)),
            ("line_start".into(), Value::Int(10)),
            ("line_end".into(), Value::Int(20)),
            ("signature".into(), s(&format!("fn {name}()"))),
            ("doc".into(), s(&format!("what {name} does"))),
        ];
        if let Some(target) = callee {
            props.push(("calls_to".into(), list(&[target.to_string()])));
            props.push(("call_lines".into(), list(&[format!("{target}\t14")])));
        }
        w.insert_node("Symbol", &key, props).expect("symbol");
    }

    for i in 0..CODE_COMMITS {
        let sha = code_sha(i);
        let author = if i % 2 == 0 {
            "a@example.test"
        } else {
            "b@example.test"
        };
        w.insert_node(
            "Commit",
            &sha,
            vec![
                ("id".into(), s(&sha)),
                ("message".into(), s(&format!("change {i:02}"))),
                ("ts".into(), Value::Int(CODE_T0 + i as i64 * 86_400)),
                ("author_id".into(), s(author)),
            ],
        )
        .expect("commit");
        for f in code_touched(i) {
            w.insert_edge("TOUCHED", &sha, f).expect("touched");
        }
    }

    w.insert_node(
        "Note",
        "note:seed",
        vec![
            ("id".into(), s("note:seed")),
            ("text".into(), s("the core module is the entry point")),
            ("kind".into(), s("note")),
            ("ts".into(), Value::Int(CODE_T0)),
            ("source".into(), s("agent")),
            ("about".into(), list(&["src/core.rs".to_string()])),
        ],
    )
    .expect("note");

    w.insert_node(
        "GitSync",
        "__mushroomdb_git_sync__",
        vec![
            ("id".into(), s("__mushroomdb_git_sync__")),
            ("sha".into(), s(&code_sha(CODE_COMMITS - 1))),
            ("synced_at".into(), Value::Int(CODE_T0 + 4 * 86_400)),
            // Deliberately not a real path: `context` must still answer from
            // the graph when the working tree it names is not there.
            ("repo".into(), s("/nonexistent/mushroomdb-test-repo")),
            ("recurse".into(), Value::Bool(false)),
            ("prs".into(), Value::Bool(false)),
            ("structure".into(), Value::Bool(true)),
            ("docs".into(), Value::Bool(true)),
        ],
    )
    .expect("gitsync");

    for (label, field) in [("File", "path"), ("Symbol", "name"), ("Note", "text")] {
        w.enable_fulltext(label, field).expect("fulltext");
    }
    for def in code_rules() {
        w.create_rule(def).expect("rule");
    }
}

/// A store shaped the way `ingest-git` leaves one, small enough to assert on.
fn code_store(name: &str) -> SharedDb {
    let db = open(name);
    seed_code_graph(&db);
    db
}

/// The text of a successful task reply, and nothing else — a task tool's reply
/// carries one text block and no `structuredContent`.
fn task_text(reply: &Js) -> String {
    assert!(
        reply.get("error").is_none() || reply["error"].is_null(),
        "expected tool result, got protocol error: {reply}"
    );
    assert!(
        !reply["result"]["isError"].as_bool().unwrap_or(false),
        "expected success, got tool error: {reply}"
    );
    assert_eq!(reply["result"]["content"][0]["type"], "text");
    assert_eq!(
        reply["result"]["content"].as_array().map(Vec::len),
        Some(1),
        "one content block, not two: {reply}"
    );
    assert!(
        reply["result"].get("structuredContent").is_none(),
        "a task tool must not ship the report beside the text: {reply}"
    );
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("content[0].text string: {reply}"))
        .to_string()
}

/// Unwrap a task tool's rendered digest.
///
/// Asserts the untrusted-data framing line on every task tool that goes through
/// it — repository content reaches an assistant here, and it has to be marked
/// as data before the assistant reads a word of it. The digest is returned
/// **without** that line, so a caller's assertions are about the render.
fn task_reply(reply: &Js) -> String {
    let text = task_text(reply);
    let body = text
        .strip_prefix(UNTRUSTED_FRAMING)
        .unwrap_or_else(|| {
            panic!("task tool text must open with the untrusted-data framing line: {text:?}")
        })
        .to_string();
    assert!(
        !body.contains(UNTRUSTED_FRAMING),
        "the framing line must be stamped once, not twice: {text:?}"
    );
    body
}

/// The report behind a task tool, asked for with `json: true`: the text content
/// *is* the serialised report, with no digest and no framing line.
fn task_report(db: SharedDb, name: &str, mut args: Js) -> Js {
    args["json"] = json!(true);
    let reply = one_task_call(db, name, args);
    let text = task_text(&reply);
    assert!(
        !text.starts_with(UNTRUSTED_FRAMING),
        "a json reply is data for a program, not prose to frame: {text}"
    );
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("json reply must parse: {e}: {text}"))
}

/// A read-only task tool's digest and report together, from two calls on the
/// same store.
fn task_both(db: SharedDb, name: &str, args: Js) -> (String, Js) {
    let text = task_reply(&one_task_call(db.clone(), name, args.clone()));
    (text, task_report(db, name, args))
}

/// The text of a tool error reply.
fn error_text(reply: &Js) -> String {
    assert_eq!(
        reply["result"]["isError"],
        json!(true),
        "expected a tool error: {reply}"
    );
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn one_task_call(db: SharedDb, name: &str, args: Js) -> Js {
    let (res, out) = exchange(db, &call(1, name, args));
    assert!(res.is_ok(), "{res:?}");
    parse_lines(&out).remove(0)
}

/// The fifteen a memory store lists, in the order it lists them: the entity
/// questions first, the store's own counts last.
const ASSOCIATION_TOOLS: [&str; 15] = [
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

/// Binding: a store no repository was ingested into lists the association
/// surface — the fifteen tools that answer a question about an entity graph,
/// in that order — and none of the code-door task tools.
#[test]
fn a_memory_store_lists_the_association_surface() {
    let (res, out) = exchange(open("list-default"), &req(json!(1), "tools/list", None));
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let tools = replies[0]["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        ASSOCIATION_TOOLS.to_vec(),
        "default tools/list on a memory store"
    );
    assert_eq!(tools.len(), 15);
    for hidden in [
        "explore", "map", "context", "impact", "owners", "why", "sync",
    ] {
        assert!(
            !names.contains(&hidden),
            "{hidden} answers from a code graph there is none of, so it must not be listed"
        );
    }
}

/// A memory store holding one `Person`, one `Org`, and the `works_at` rule
/// that derives a `WORKS_AT` edge between them.
fn association_store(name: &str) -> SharedDb {
    let db = open(name);
    {
        let mut w = db.write();
        w.insert_node(
            "Org",
            "acme",
            vec![("id".into(), Value::Str("acme".into()))],
        )
        .unwrap();
        w.insert_node(
            "Person",
            "p1",
            vec![
                ("id".into(), Value::Str("p1".into())),
                ("org_id".into(), Value::Str("acme".into())),
            ],
        )
        .unwrap();
        w.create_rule(core_api::RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: core_api::Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
    }
    db
}

/// Binding: `explain_association` answers in prose like every other task tool,
/// and hands back the same array of explanations to a caller that asks for the
/// report with `json: true`.
#[test]
fn explain_association_answers_with_text_and_json_on_request() {
    let db = association_store("explain-text");
    let text = task_reply(&one_task_call(
        db.clone(),
        "explain_association",
        json!({"a": "p1", "b": "acme"}),
    ));
    assert!(
        text.starts_with("mushroomdb explain — p1 ↔ acme: 1 relationship(s)"),
        "{text}"
    );
    assert!(text.contains("WORKS_AT via rule works_at"), "{text}");
    assert!(text.contains("key_match on org_id"), "{text}");

    let report = task_report(db, "explain_association", json!({"a": "p1", "b": "acme"}));
    assert!(report.is_array(), "{report}");
    assert_eq!(report[0]["rule"], json!("works_at"));
    assert_eq!(report[0]["edge_type"], json!("WORKS_AT"));
}

/// A store whose talent and company overlap on a list, agree on a field, sit
/// a known distance apart, and carry a numeric field within tolerance — one
/// rule per predicate kind, so one `explain_association` call exercises all
/// four evidence shapes at once.
fn evidence_store(name: &str) -> SharedDb {
    let db = open(name);
    {
        let mut w = db.write();
        w.insert_node(
            "Talent",
            "t1",
            vec![
                ("id".into(), Value::Str("t1".into())),
                ("industry".into(), Value::Str("Architecture".into())),
                (
                    "specialties".into(),
                    Value::List(vec![
                        Value::Str("hospitality".into()),
                        Value::Str("residential".into()),
                        Value::Str("retail".into()),
                    ]),
                ),
                (
                    "location".into(),
                    Value::List(vec![Value::Float(40.7128), Value::Float(-74.006)]),
                ),
                ("size_bucket".into(), Value::Int(4)),
            ],
        )
        .unwrap();
        w.insert_node(
            "Company",
            "c1",
            vec![
                ("id".into(), Value::Str("c1".into())),
                ("industry".into(), Value::Str("Architecture".into())),
                (
                    "specialties".into(),
                    Value::List(vec![
                        Value::Str("commercial".into()),
                        Value::Str("hospitality".into()),
                        Value::Str("residential".into()),
                        Value::Str("single-family".into()),
                    ]),
                ),
                (
                    "location".into(),
                    Value::List(vec![Value::Float(40.73061), Value::Float(-73.8)]),
                ),
                ("size_bucket".into(), Value::Int(5)),
            ],
        )
        .unwrap();
        let rule =
            |name: &str, edge_type: &str, predicate: core_api::Predicate| core_api::RuleDef {
                name: name.into(),
                src_label: "Talent".into(),
                dst_label: "Company".into(),
                predicate,
                edge_type: edge_type.into(),
                weight_prop: None,
                max_edges: None,
                approximate: false,
                via_label: None,
                via_edge: None,
                via_dir: None,
            };
        w.create_rule(rule(
            "specialty_match",
            "SPECIALTY_MATCH",
            core_api::Predicate::Overlap {
                field: "specialties".into(),
                min: 0.2,
            },
        ))
        .unwrap();
        w.create_rule(rule(
            "industry_alignment",
            "INDUSTRY_ALIGNMENT",
            core_api::Predicate::FieldEqual {
                field: "industry".into(),
            },
        ))
        .unwrap();
        w.create_rule(rule(
            "location_fit",
            "LOCATION_FIT",
            core_api::Predicate::GeoRadius {
                field: "location".into(),
                km: 50.0,
            },
        ))
        .unwrap();
        w.create_rule(rule(
            "size_fit",
            "SIZE_FIT",
            core_api::Predicate::NumericWithin {
                field: "size_bucket".into(),
                tolerance: 2.0,
            },
        ))
        .unwrap();
    }
    db
}

/// Binding: every explained edge names the values the two nodes actually
/// share, not just the rule and the threshold.
///
/// This is the whole point of the tool. Without these values the agent that
/// asked "which specialties do they have in common" calls `node_info` on both
/// nodes and reads out every specialty either one holds — `retail` and
/// `commercial` and `single-family` included, none of which is shared.
#[test]
fn explain_association_names_the_values_the_two_share() {
    let db = evidence_store("explain-evidence");
    let text = task_reply(&one_task_call(
        db.clone(),
        "explain_association",
        json!({"a": "t1", "b": "c1"}),
    ));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0], "mushroomdb explain — t1 ↔ c1: 4 relationship(s)",
        "{text}"
    );
    // Edges are listed by edge type, the order `explain` returns them in.
    assert_eq!(
        lines[1],
        "  INDUSTRY_ALIGNMENT via rule industry_alignment (score 1.00) — field_equal on industry \
         [industry: Architecture]",
        "{text}"
    );
    assert_eq!(
        lines[2],
        "  LOCATION_FIT via rule location_fit (score 0.65) — geo_radius on location within 50 km \
         [location: 40.7128,-74.0060 vs 40.7306,-73.8000, 17.47 km apart]",
        "{text}"
    );
    assert_eq!(
        lines[3],
        "  SIZE_FIT via rule size_fit (score 0.50) — numeric_within on size_bucket +/- 2 \
         [size_bucket: 4 vs 5]",
        "{text}"
    );
    assert_eq!(
        lines[4],
        "  SPECIALTY_MATCH via rule specialty_match (score 0.40) — overlap on specialties >= 0.2 \
         [specialties: hospitality, residential]",
        "{text}"
    );

    // The forbidden half of the answer: a value only one of the two holds
    // must never appear in the reply.
    for unshared in ["retail", "commercial", "single-family"] {
        assert!(
            !text.contains(unshared),
            "`{unshared}` is held by one node only and must not be named: {text}"
        );
    }

    // A why reply stays a screen, not a dump.
    assert!(
        text.len() < 600,
        "a four-relationship reply must stay compact, was {} bytes: {text}",
        text.len()
    );
}

/// Binding: the `json: true` shape gains an `evidence` object per
/// relationship, one shape per predicate kind.
#[test]
fn explain_association_report_carries_an_evidence_object_per_relationship() {
    let db = evidence_store("explain-evidence-json");
    let report = task_report(db, "explain_association", json!({"a": "t1", "b": "c1"}));
    let rows = report.as_array().expect("array");
    assert_eq!(rows.len(), 4, "{report}");
    let by_type = |t: &str| {
        rows.iter()
            .find(|r| r["edge_type"] == json!(t))
            .unwrap_or_else(|| panic!("no {t} edge: {report}"))["evidence"]
            .clone()
    };
    assert_eq!(
        by_type("SPECIALTY_MATCH"),
        json!({"field": "specialties", "shared": ["hospitality", "residential"]})
    );
    assert_eq!(
        by_type("INDUSTRY_ALIGNMENT"),
        json!({"field": "industry", "value": "Architecture"})
    );
    assert_eq!(
        by_type("LOCATION_FIT"),
        json!({
            "field": "location",
            "a": "40.7128,-74.0060",
            "b": "40.7306,-73.8000",
            "km": 17.47
        })
    );
    assert_eq!(
        by_type("SIZE_FIT"),
        json!({"field": "size_bucket", "a": 4, "b": 5})
    );
    // Nothing else about the report moved.
    assert_eq!(
        by_type_field(rows, "SPECIALTY_MATCH", "rule"),
        json!("specialty_match")
    );
    assert_eq!(
        by_type_field(rows, "SPECIALTY_MATCH", "src_key"),
        json!("t1")
    );
    assert_eq!(
        by_type_field(rows, "SPECIALTY_MATCH", "dst_key"),
        json!("c1")
    );
}

fn by_type_field(rows: &[Js], edge_type: &str, field: &str) -> Js {
    rows.iter()
        .find(|r| r["edge_type"] == json!(edge_type))
        .expect("edge")[field]
        .clone()
}

/// Binding: a composed predicate reports one clause of evidence per branch
/// that matched, on the one line.
#[test]
fn composed_predicate_evidence_lists_every_branch() {
    let db = open("explain-evidence-all");
    {
        let mut w = db.write();
        w.insert_node(
            "Talent",
            "t1",
            vec![
                ("id".into(), Value::Str("t1".into())),
                ("industry".into(), Value::Str("Architecture".into())),
                (
                    "specialties".into(),
                    Value::List(vec![
                        Value::Str("hospitality".into()),
                        Value::Str("retail".into()),
                    ]),
                ),
            ],
        )
        .unwrap();
        w.insert_node(
            "Company",
            "c1",
            vec![
                ("id".into(), Value::Str("c1".into())),
                ("industry".into(), Value::Str("Architecture".into())),
                (
                    "specialties".into(),
                    Value::List(vec![
                        Value::Str("civic".into()),
                        Value::Str("hospitality".into()),
                    ]),
                ),
            ],
        )
        .unwrap();
        w.create_rule(core_api::RuleDef {
            name: "fit".into(),
            src_label: "Talent".into(),
            dst_label: "Company".into(),
            predicate: core_api::Predicate::All(vec![
                core_api::Predicate::FieldEqual {
                    field: "industry".into(),
                },
                core_api::Predicate::Overlap {
                    field: "specialties".into(),
                    min: 0.2,
                },
            ]),
            edge_type: "FIT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
    }
    let text = task_reply(&one_task_call(
        db.clone(),
        "explain_association",
        json!({"a": "t1", "b": "c1"}),
    ));
    assert!(
        text.contains("[industry: Architecture; specialties: hospitality]"),
        "{text}"
    );
    assert!(
        !text.contains("retail") && !text.contains("civic"),
        "{text}"
    );

    let report = task_report(db, "explain_association", json!({"a": "t1", "b": "c1"}));
    assert_eq!(
        report[0]["evidence"],
        json!({"parts": [
            {"field": "industry", "value": "Architecture"},
            {"field": "specialties", "shared": ["hospitality"]}
        ]})
    );
}

/// Binding: an `any` predicate reports only the branches that actually
/// matched — the unsatisfied ones are the reasons the engine *rejected*, and
/// printing them stated a falsehood about why the two are related.
#[test]
fn any_predicate_evidence_lists_only_the_satisfied_branches() {
    let db = open("explain-evidence-any");
    {
        let mut w = db.write();
        let node = |industry: &str, bucket: i64, specialties: Vec<&str>| {
            vec![
                ("industry".into(), Value::Str(industry.into())),
                ("size_bucket".into(), Value::Int(bucket)),
                (
                    "specialties".into(),
                    Value::List(
                        specialties
                            .into_iter()
                            .map(|s| Value::Str(s.into()))
                            .collect(),
                    ),
                ),
            ]
        };
        w.insert_node(
            "Talent",
            "t1",
            node("Architecture", 1, vec!["a", "b", "c", "d"]),
        )
        .unwrap();
        // Same industry (the branch that matches), a size bucket eight apart
        // on a ±2 tolerance, and a specialties overlap of 1/7 under a 0.5 min
        // — two branches the engine rejected.
        w.insert_node(
            "Company",
            "c1",
            node("Architecture", 9, vec!["d", "e", "f", "g"]),
        )
        .unwrap();
        w.create_rule(core_api::RuleDef {
            name: "fit_any".into(),
            src_label: "Talent".into(),
            dst_label: "Company".into(),
            predicate: core_api::Predicate::Any(vec![
                core_api::Predicate::FieldEqual {
                    field: "industry".into(),
                },
                core_api::Predicate::NumericWithin {
                    field: "size_bucket".into(),
                    tolerance: 2.0,
                },
                core_api::Predicate::Overlap {
                    field: "specialties".into(),
                    min: 0.5,
                },
            ]),
            edge_type: "FIT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
    }

    let text = task_reply(&one_task_call(
        db.clone(),
        "explain_association",
        json!({"a": "t1", "b": "c1"}),
    ));
    assert!(text.contains("[industry: Architecture]"), "{text}");
    assert!(
        !text.contains("1 vs 9"),
        "an out-of-tolerance branch is not evidence: {text}"
    );
    assert!(
        !text.contains("specialties: d"),
        "an overlap under the rule's min is not evidence: {text}"
    );

    let report = task_report(db, "explain_association", json!({"a": "t1", "b": "c1"}));
    assert_eq!(
        report[0]["evidence"],
        json!({"parts": [{"field": "industry", "value": "Architecture"}]})
    );
}

/// Binding: a key-match rule's shared value is the destination's own key.
#[test]
fn key_match_evidence_names_the_destination_key() {
    let db = association_store("explain-evidence-keymatch");
    let text = task_reply(&one_task_call(
        db.clone(),
        "explain_association",
        json!({"a": "p1", "b": "acme"}),
    ));
    assert!(text.contains("[org_id: acme]"), "{text}");
    let report = task_report(db, "explain_association", json!({"a": "p1", "b": "acme"}));
    assert_eq!(
        report[0]["evidence"],
        json!({"field": "org_id", "value": "acme"})
    );
}

/// Binding: two keys the graph holds with no rule edge between them are an
/// answer, not a failure.
#[test]
fn explain_association_says_none_when_nothing_links_the_two() {
    let db = association_store("explain-none");
    seed_person(&db, "p2");
    let text = task_reply(&one_task_call(
        db,
        "explain_association",
        json!({"a": "p2", "b": "acme"}),
    ));
    assert!(
        text.starts_with("mushroomdb explain — p2 ↔ acme: 0 relationship(s)"),
        "{text}"
    );
    assert!(text.contains("\n  none"), "{text}");
}

// ── node_edges / neighborhood ────────────────────────────────────────────────

/// Binding: `node_edges` answers in prose, grouped by edge type with a count,
/// and every derived edge carries its direction, rule, score and predicate —
/// the evidence that used to cost a second `explain` call.
#[test]
fn node_edges_groups_by_type_and_names_the_rule_behind_each_edge() {
    let db = association_store("edges-grouped");
    db.write()
        .insert_node("Person", "p2", vec![("id".into(), Value::Str("p2".into()))])
        .unwrap();
    db.write().insert_edge("KNOWS", "p2", "p1").unwrap();

    let (text, report) = task_both(db, "node_edges", json!({"key": "p1"}));
    assert_eq!(
        text,
        "mushroomdb edges — p1: 2 edge(s) over 2 type(s)\n\
         KNOWS (1)\n\
         \x20 ← p2\n\
         WORKS_AT (1)\n\
         \x20 → acme  rule works_at  score 1.00 — key_match on org_id\n",
        "{text}"
    );

    assert_eq!(report["key"], json!("p1"));
    assert_eq!(report["total"], json!(2));
    let works_at = &report["types"][1];
    assert_eq!(works_at["edge_type"], json!("WORKS_AT"));
    assert_eq!(works_at["edges"][0]["rule"], json!("works_at"));
    assert_eq!(works_at["edges"][0]["direction"], json!("out"));
    assert_eq!(
        works_at["edges"][0]["predicate"],
        json!("key_match on org_id")
    );
    // A manual edge was written by a caller, not matched by a predicate, so it
    // has nothing to explain and claims no rule.
    assert_eq!(report["types"][0]["edges"][0]["rule"], json!(null));
}

/// A store where one `Person` is joined to `n` `Org`s by the same rule, which
/// is the shape a listing has to cap.
fn wide_store(name: &str, n: usize) -> SharedDb {
    let db = open(name);
    {
        let mut w = db.write();
        for i in 0..n {
            w.insert_node(
                "Org",
                &format!("org{i:02}"),
                vec![("id".into(), Value::Str(format!("org{i:02}")))],
            )
            .unwrap();
        }
        w.create_rule(core_api::RuleDef {
            name: "any_org".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: core_api::Predicate::FieldEqual {
                field: "sector".into(),
            },
            edge_type: "IN_SECTOR".into(),
            weight_prop: None,
            max_edges: Some(1000),
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
        for i in 0..n {
            w.set_prop(&format!("org{i:02}"), "sector", Value::Str("tech".into()))
                .unwrap();
        }
        w.insert_node(
            "Person",
            "p1",
            vec![
                ("id".into(), Value::Str("p1".into())),
                ("sector".into(), Value::Str("tech".into())),
            ],
        )
        .unwrap();
    }
    db
}

/// Binding: a type with more edges than the limit lists the limit and counts
/// the rest, and the header still says how many there are in total.
#[test]
fn node_edges_caps_each_type_and_counts_what_it_did_not_list() {
    let db = wide_store("edges-wide", 25);
    let text = task_reply(&one_task_call(
        db.clone(),
        "node_edges",
        json!({"key": "p1"}),
    ));
    assert!(
        text.starts_with("mushroomdb edges — p1: 25 edge(s) over 1 type(s)\nIN_SECTOR (25)\n"),
        "the header counts every edge, listed or not: {text}"
    );
    assert_eq!(
        text.lines().filter(|l| l.starts_with("  →")).count(),
        10,
        "ten edges listed by default: {text}"
    );
    assert!(text.contains("  … and 15 more\n"), "{text}");

    // And an explicit limit moves both numbers.
    let report = task_report(db, "node_edges", json!({"key": "p1", "limit": 3}));
    assert_eq!(report["types"][0]["count"], json!(25));
    assert_eq!(report["types"][0]["listed"], json!(3));
    assert_eq!(
        report["types"][0]["edges"].as_array().map(Vec::len),
        Some(3)
    );
}

/// Binding: `edge_type` narrows the reply to one type's partner keys, and a
/// limit outside the schema's range is a tool error rather than a silent
/// clamp to nothing.
#[test]
fn node_edges_filters_by_type_and_refuses_a_zero_limit() {
    let db = association_store("edges-filter");
    db.write()
        .insert_node("Person", "p2", vec![("id".into(), Value::Str("p2".into()))])
        .unwrap();
    db.write().insert_edge("KNOWS", "p2", "p1").unwrap();

    let report = task_report(
        db.clone(),
        "node_edges",
        json!({"key": "p1", "edge_type": "WORKS_AT"}),
    );
    assert_eq!(report["edge_type"], json!("WORKS_AT"));
    assert_eq!(report["edges"], json!(1));
    assert_eq!(report["total"], json!(1));
    assert_eq!(report["rule"], json!("works_at"));
    assert_eq!(report["partners"], json!(["acme"]));

    let reply = one_task_call(db.clone(), "node_edges", json!({"key": "p1", "limit": 0}));
    assert!(error_text(&reply).contains("positive integer"), "{reply}");

    let reply = one_task_call(db, "node_edges", json!({"key": "ghost"}));
    assert!(error_text(&reply).contains("ghost"), "{reply}");
}

/// Binding: the grouped report carries the top-level `listed` the docs and the
/// tool description promise, and echoes the `label` it was narrowed by.
///
/// `listed` is the sum of the per-type counts — how many edges are actually in
/// the document — so a caller can tell a whole reply from a cut one without
/// adding the types up itself. And a report that was narrowed by a label and
/// does not say so reads as the unnarrowed answer: every other shape of this
/// reply echoes it, and the grouped one used to be the exception.
#[test]
fn the_grouped_edge_report_counts_what_it_listed_and_echoes_its_label() {
    let db = wide_store("edges-grouped-listed", 25);

    let report = task_report(db.clone(), "node_edges", json!({"key": "p1", "limit": 4}));
    assert_eq!(report["total"], json!(25));
    assert_eq!(report["listed"], json!(4), "{report}");
    assert_eq!(report["types"][0]["listed"], json!(4));
    assert_eq!(
        report["label"],
        json!(null),
        "a call that named no label says nothing about one"
    );

    let report = task_report(
        db.clone(),
        "node_edges",
        json!({"key": "p1", "label": "Org", "limit": 4}),
    );
    assert_eq!(report["label"], json!("Org"), "{report}");
    assert_eq!(report["listed"], json!(4));
    assert_eq!(report["total"], json!(25));

    // A label nothing carries narrows the counts, not just the listings.
    let report = task_report(db, "node_edges", json!({"key": "p1", "label": "Nobody"}));
    assert_eq!(report["label"], json!("Nobody"));
    assert_eq!(report["total"], json!(0));
    assert_eq!(report["listed"], json!(0));
}

/// Binding: `neighborhood` honours `label` rather than accepting and ignoring
/// it — at depth 1, where it shares `node_edges`' grouped renderer, and past
/// it, where the reply is the traversal table.
///
/// The walk itself is never narrowed: a hop through a node of another label is
/// how a two-hop question reaches the label it asked about.
#[test]
fn neighborhood_narrows_by_label_at_both_depths() {
    let db = all_of_store("neighborhood-label");
    // A second hop off `o1`, of a label the hub itself has none of.
    {
        let mut w = db.write();
        w.insert_node("Note", "n1", vec![("id".into(), Value::Str("n1".into()))])
            .unwrap();
        w.insert_edge("A", "o1", "n1").unwrap();
    }

    // Depth 1: the grouped listing, counts included. `o2` is a `Job`, so a
    // `Company` filter keeps o1 and o3 and drops it.
    let report = task_report(
        db.clone(),
        "neighborhood",
        json!({"key": "hub", "label": "Company"}),
    );
    assert_eq!(report["label"], json!("Company"), "{report}");
    assert_eq!(
        report["total"],
        json!(5),
        "o1 and o3 over A and B, plus hub→o1 by C; o2's three edges gone: {report}"
    );

    let text = task_reply(&one_task_call(
        db.clone(),
        "neighborhood",
        json!({"key": "hub", "label": "Company"}),
    ));
    assert!(
        !text.contains("o2"),
        "the Job partner is filtered out: {text}"
    );
    assert!(text.contains("o1") && text.contains("o3"), "{text}");

    // Depth 2: the traversal table, narrowed to the rows of that label — and
    // the walk still went through `o1` (a Company) to reach `n1`.
    let reply = one_task_call(
        db.clone(),
        "neighborhood",
        json!({"key": "hub", "depth": 2, "label": "Note"}),
    );
    let table = |reply: &Js| -> Vec<String> {
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .expect("a text content");
        let doc: Js = serde_json::from_str(text).expect("a result set");
        doc["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|r| r[0].as_str().expect("key").to_string())
            .collect()
    };
    assert_eq!(table(&reply), vec!["n1".to_string()], "{reply}");

    // Without the label the same walk reports every node it reached.
    let reply = one_task_call(db, "neighborhood", json!({"key": "hub", "depth": 2}));
    assert!(table(&reply).len() > 1, "{reply}");
}

/// Binding: an edge type that is nowhere in the store is a tool error naming
/// it and the types the store does have — not "0 edges", which is the right
/// answer for a type that exists and this node has none of.
///
/// A caller cannot tell those two apart from a count, so a misspelling used to
/// read as a fact about the graph: the association benchmark's own failure
/// mode, an agent concluding a node had no partners of a type it had
/// mistyped.
#[test]
fn a_misspelled_edge_type_names_itself_and_the_ones_that_exist() {
    let db = all_of_store("edges-misspelled");

    let reply = one_task_call(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "edge_type": "AA"}),
    );
    let err = error_text(&reply);
    assert!(err.contains("no edge type named AA"), "{err}");
    assert!(
        err.contains("A") && err.contains("B") && err.contains("C"),
        "the error carries what to ask instead: {err}"
    );

    let reply = one_task_call(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A", "NOPE"]}),
    );
    let err = error_text(&reply);
    assert!(err.contains("no edge type named NOPE"), "{err}");

    // A type that exists and this node has none of in that direction is still
    // an answer, never an error.
    let report = task_report(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A", "B", "C"], "direction": "in"}),
    );
    assert_eq!(report["total"], json!(0));

    // And a node with no edges of a real type answers zero, not an error.
    let report = task_report(db, "node_edges", json!({"key": "o4", "edge_type": "A"}));
    assert_eq!(report["total"], json!(0), "{report}");
}

/// Binding: one `edge_type` prints the partners as keys, on wrapped lines,
/// with the rule named once in the header rather than once per partner — and
/// `limit` cuts the list and says what it cut.
#[test]
fn node_edges_with_one_type_prints_partner_keys_and_the_rule_once() {
    let db = wide_store("edges-keys-only", 25);

    let text = task_reply(&one_task_call(
        db.clone(),
        "node_edges",
        json!({"key": "p1", "edge_type": "IN_SECTOR"}),
    ));
    assert!(
        text.contains("mushroomdb edges — p1: 25 edge(s) over 1 type(s)\n"),
        "{text}"
    );
    assert!(
        text.contains("IN_SECTOR (25, rule any_org): org00,"),
        "the rule is named once, in the header: {text}"
    );
    assert_eq!(
        text.matches("rule any_org").count(),
        1,
        "the rule is not repeated per partner: {text}"
    );
    assert!(text.contains("org24"), "every partner is listed: {text}");
    // Wrapped, not one enormous line, and no line runs away.
    let widest = text.lines().map(str::len).max().unwrap_or(0);
    assert!(widest <= 120, "line of {widest} bytes: {text}");
    assert!(
        text.lines().filter(|l| l.contains("org")).count() >= 2,
        "the key list wraps: {text}"
    );

    let text = task_reply(&one_task_call(
        db,
        "node_edges",
        json!({"key": "p1", "edge_type": "IN_SECTOR", "limit": 3}),
    ));
    assert!(
        text.contains("IN_SECTOR (25, rule any_org): org00, org01, org02\n"),
        "{text}"
    );
    assert!(text.contains("… and 22 more\n"), "{text}");
}

/// A hub joined to four partners by three edge types, arranged so that the
/// intersection over the types is a smaller set than any one of them:
///
/// - `A` and `B`: hub → o1, o2, o3
/// - `C`: hub → o1, and o2 → hub (incoming, so direction tells them apart)
///
/// o4 is joined by nothing and must never appear.
fn all_of_store(name: &str) -> SharedDb {
    let db = open(name);
    {
        let mut w = db.write();
        // o2 is the odd one out by label as well as by direction, so a
        // `label` filter and a `direction` filter cut the set differently.
        for (k, label) in [
            ("hub", "Talent"),
            ("o1", "Company"),
            ("o2", "Job"),
            ("o3", "Company"),
            ("o4", "Company"),
        ] {
            w.insert_node(label, k, vec![("id".into(), Value::Str(k.into()))])
                .unwrap();
        }
        for partner in ["o1", "o2", "o3"] {
            w.insert_edge("A", "hub", partner).unwrap();
            w.insert_edge("B", "hub", partner).unwrap();
        }
        w.insert_edge("C", "hub", "o1").unwrap();
        w.insert_edge("C", "o2", "hub").unwrap();
    }
    db
}

/// Binding: `all_of` answers with the partners carrying EVERY named type —
/// a partner with two of three is not one of them — and `direction` decides
/// which edges count toward that.
#[test]
fn node_edges_all_of_is_the_intersection_over_the_types() {
    let db = all_of_store("edges-all-of");

    let (text, report) = task_both(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A", "B", "C"]}),
    );
    assert_eq!(
        text, "mushroomdb edges — hub — partners linked by all of A, B, C: 2\no1, o2\n",
        "{text}"
    );
    assert_eq!(report["key"], json!("hub"));
    assert_eq!(report["all_of"], json!(["A", "B", "C"]));
    assert_eq!(report["partners"], json!(["o1", "o2"]));
    assert_eq!(report["total"], json!(2));
    assert_eq!(report["at"], json!(null), "a live call names no commit");

    // Two of three is not all of three.
    let report = task_report(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A", "B"]}),
    );
    assert_eq!(report["partners"], json!(["o1", "o2", "o3"]));

    // Direction filters the edges the intersection is taken over: o2's only
    // `C` edge points at the hub, so it is out of the outgoing answer.
    let report = task_report(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A", "B", "C"], "direction": "out"}),
    );
    assert_eq!(report["partners"], json!(["o1"]));

    let report = task_report(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A", "B", "C"], "direction": "in"}),
    );
    assert_eq!(report["partners"], json!([]));
    assert_eq!(report["total"], json!(0));

    let text = task_reply(&one_task_call(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A", "B", "C"], "direction": "in"}),
    ));
    assert!(text.contains("all of A, B, C: 0\n  none\n"), "{text}");

    // An empty `all_of` is a call that means nothing, and an unknown
    // direction is refused rather than quietly read as "any".
    let reply = one_task_call(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": []}),
    );
    assert!(
        error_text(&reply).contains("at least one edge type"),
        "{reply}"
    );
    let reply = one_task_call(
        db,
        "node_edges",
        json!({"key": "hub", "all_of": ["A"], "direction": "sideways"}),
    );
    assert!(error_text(&reply).contains("sideways"), "{reply}");
}

/// Binding: `label` narrows the partners in every form — the intersection,
/// the one-type listing and the grouped view — counts included, which is what
/// makes "which *companies* was T linked to" one call rather than a filter
/// applied by hand afterwards.
#[test]
fn label_narrows_the_partners_in_every_form() {
    let db = all_of_store("edges-label");

    // The intersection, then the same intersection restricted by label.
    let report = task_report(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A", "B", "C"]}),
    );
    assert_eq!(report["partners"], json!(["o1", "o2"]));

    let (text, report) = task_both(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A", "B", "C"], "label": "Company"}),
    );
    assert_eq!(report["partners"], json!(["o1"]), "o2 is a Job: {report}");
    assert_eq!(report["total"], json!(1), "the count is narrowed too");
    assert_eq!(report["label"], json!("Company"));
    assert!(
        text.contains("partners linked by all of A, B, C: 1\no1\n"),
        "{text}"
    );

    // Two types, both companies.
    let report = task_report(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A", "B"], "label": "Company"}),
    );
    assert_eq!(report["partners"], json!(["o1", "o3"]));

    // One `edge_type`: three partners, two of them companies.
    let report = task_report(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "edge_type": "A"}),
    );
    assert_eq!(report["partners"], json!(["o1", "o2", "o3"]));
    let (text, report) = task_both(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "edge_type": "A", "label": "Company"}),
    );
    assert_eq!(report["partners"], json!(["o1", "o3"]));
    assert_eq!(report["edges"], json!(2), "the edge count is narrowed too");
    assert!(text.contains("A (2): o1, o3\n"), "{text}");

    // And the grouped view, where the header's totals move with the filter.
    let (text, report) = task_both(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "label": "Company"}),
    );
    assert_eq!(
        report["total"],
        json!(5),
        "A+B for o1/o3, C for o1: {report}"
    );
    assert!(
        text.starts_with("mushroomdb edges — hub: 5 edge(s) over 3 type(s)\n"),
        "{text}"
    );
    assert!(!text.contains("o2"), "a Job is not a Company: {text}");

    // A label nothing carries is an empty answer, not an error.
    let report = task_report(
        db,
        "node_edges",
        json!({"key": "hub", "all_of": ["A"], "label": "Ghost"}),
    );
    assert_eq!(report["partners"], json!([]));
    assert_eq!(report["total"], json!(0));
}

/// Binding: `edges_at` takes the same `label`, at a past commit.
#[test]
fn edges_at_label_narrows_the_partners_at_a_past_commit() {
    let db = all_of_store("edges-at-label");
    let at = db.read().wal_total_commits().unwrap() - 1;

    let (text, report) = task_both(
        db.clone(),
        "edges_at",
        json!({"key": "hub", "at": at, "all_of": ["A", "B", "C"], "label": "Company"}),
    );
    assert_eq!(report["partners"], json!(["o1"]));
    assert_eq!(report["total"], json!(1));
    assert_eq!(report["label"], json!("Company"));
    assert!(
        text.contains(&format!(
            "hub as of commit {at} — partners linked by all of A, B, C: 1\no1\n"
        )),
        "{text}"
    );

    let (text, report) = task_both(
        db,
        "edges_at",
        json!({"key": "hub", "at": at, "edge_type": "A", "label": "Company"}),
    );
    assert_eq!(report["partners"], json!(["o1", "o3"]));
    assert_eq!(report["edges"], json!(2));
    assert!(text.contains("A (2): o1, o3\n"), "{text}");
}

/// Binding: `what_if` takes the same `label`, and it narrows the counts the
/// header states, not just the keys under them.
#[test]
fn what_if_label_narrows_both_sides() {
    let (db, dir) = what_if_store("what-if-label");

    let text = task_reply(&what_if_call(
        db.clone(),
        &dir,
        json!({"key": "p1", "field": "org_id", "value": "globex", "label": "Org"}),
    ));
    assert!(text.contains("would lose 1, would gain 1"), "{text}");

    let reply = what_if_call(
        db.clone(),
        &dir,
        json!({"key": "p1", "field": "org_id", "value": "globex", "label": "Person", "json": true}),
    );
    let report: Js = serde_json::from_str(
        reply["result"]["content"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
    assert_eq!(report["lost_total"], json!(0), "acme is an Org: {report}");
    assert_eq!(report["gained_total"], json!(0));
    assert_eq!(report["label"], json!("Person"));

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binding: the keys-only listing honours `limit` and counts what it cut.
#[test]
fn all_of_lists_at_most_limit_partners_and_counts_the_rest() {
    let db = wide_store("all-of-limit", 25);
    let (text, report) = task_both(
        db,
        "node_edges",
        json!({"key": "p1", "all_of": ["IN_SECTOR"], "limit": 4}),
    );
    assert!(
        text.starts_with("mushroomdb edges — p1 — partners linked by all of IN_SECTOR: 25\n"),
        "the header counts the whole set: {text}"
    );
    assert!(text.contains("org00, org01, org02, org03\n"), "{text}");
    assert!(text.contains("… and 21 more\n"), "{text}");
    assert_eq!(report["total"], json!(25));
    assert_eq!(
        report["listed"],
        json!(4),
        "a cut report says what it listed"
    );
    assert_eq!(
        report["partners"].as_array().map(Vec::len),
        Some(4),
        "{report}"
    );
}

/// Binding: a depth-1 `neighborhood` is the edge listing with its evidence;
/// past one hop it is still the breadth-first table, because no single rule
/// accounts for a two-hop row.
#[test]
fn neighborhood_answers_one_hop_with_edges_and_deeper_with_the_table() {
    let db = association_store("nb-depth");
    db.write()
        .insert_node("Person", "p2", vec![("id".into(), Value::Str("p2".into()))])
        .unwrap();
    db.write().insert_edge("KNOWS", "p1", "p2").unwrap();

    let text = task_reply(&one_task_call(
        db.clone(),
        "neighborhood",
        json!({"key": "p1"}),
    ));
    assert!(
        text.starts_with("mushroomdb edges — p1: 2 edge(s) over 2 type(s)"),
        "{text}"
    );
    assert!(text.contains("rule works_at"), "{text}");

    // The filters still apply at depth 1.
    let report = task_report(
        db.clone(),
        "neighborhood",
        json!({"key": "p1", "edge_types": ["KNOWS"], "direction": "out"}),
    );
    assert_eq!(report["total"], json!(1));
    assert_eq!(report["types"][0]["edge_type"], json!("KNOWS"));

    let reply = one_task_call(db, "neighborhood", json!({"key": "p1", "depth": 2}));
    let table = content_json(&reply);
    assert_eq!(table["columns"], json!(["key", "label", "depth"]));
}

// ── edges_at ─────────────────────────────────────────────────────────────────

/// Binding: `edges_at` checks its arguments before touching the graph.
#[test]
fn edges_at_checks_its_arguments() {
    let db = association_store("edges-at-args");
    let reply = one_task_call(db.clone(), "edges_at", json!({"at": 0}));
    assert!(error_text(&reply).contains("missing key"), "{reply}");

    let reply = one_task_call(db, "edges_at", json!({"key": "p1"}));
    assert!(error_text(&reply).contains("missing at"), "{reply}");
}

/// Binding: `edges_at` answers from the engine now — an edge is listed at the
/// commit it was still live and gone from the commit after it was deleted —
/// and an out-of-range commit is a clear tool error naming the valid range.
#[test]
fn edges_at_answers_from_the_engine_at_a_past_commit() {
    let db = open("edges-at");
    {
        let mut w = db.write();
        w.insert_node("Person", "a", vec![]).unwrap();
        w.insert_node("Person", "b", vec![]).unwrap();
        w.insert_edge("Knows", "a", "b").unwrap();
    }
    let linked = db.read().wal_total_commits().unwrap() - 1;
    db.write().delete_edge("Knows", "a", "b").unwrap();
    let gone = db.read().wal_total_commits().unwrap() - 1;

    // At the commit the edge was still live, it is listed with its direction.
    let text = task_reply(&one_task_call(
        db.clone(),
        "edges_at",
        json!({"key": "a", "at": linked}),
    ));
    assert!(
        text.starts_with(&format!(
            "mushroomdb edges_at — a as of commit {linked}: 1 edge(s)"
        )),
        "{text}"
    );
    assert!(text.contains("Knows (1)"), "{text}");
    assert!(text.contains("→ b"), "{text}");

    // After the delete, the same node has none at the later commit.
    let text = task_reply(&one_task_call(
        db.clone(),
        "edges_at",
        json!({"key": "a", "at": gone}),
    ));
    assert!(
        text.starts_with(&format!(
            "mushroomdb edges_at — a as of commit {gone}: 0 edge(s)"
        )),
        "{text}"
    );

    // `json: true` reports the same edge as a document.
    let report = task_report(db.clone(), "edges_at", json!({"key": "a", "at": linked}));
    assert_eq!(report["key"], json!("a"));
    assert_eq!(report["at"], json!(linked));
    assert_eq!(report["edges"][0]["edge_type"], json!("Knows"));
    assert_eq!(report["edges"][0]["src"], json!("a"));
    assert_eq!(report["edges"][0]["dst"], json!("b"));
    assert_eq!(report["edges"][0]["derived"], json!(false));
    assert_eq!(report["edges"][0]["rule"], json!(null));

    // Out of range is a tool error that names the valid range.
    let total = db.read().wal_total_commits().unwrap();
    let reply = one_task_call(db, "edges_at", json!({"key": "a", "at": total}));
    let msg = error_text(&reply);
    assert!(msg.contains("out of range"), "{msg}");
    assert!(msg.contains(&total.to_string()), "{msg}");
}

/// Binding: after a rename, `edges_at` queried by the node's *current* key
/// still shows edges written under its old name — with both sides
/// canonicalized to the current key. That canonicalization is exactly why
/// comparing a caller's `key` directly against a returned `src_key`/`dst_key`
/// is unsafe in general: the endpoint that used to equal `key` no longer
/// does, once `key` itself is stale. Two edges to two different partners so
/// there is something to triangulate self from (`canonical_self`'s own unit
/// test in `mcp_tasks.rs` covers the intersection logic directly, including
/// the single-edge case, which is symmetric and cannot be resolved from the
/// edges alone).
///
/// Querying by the *old*, now-stale key is checked too — verified against
/// the real engine rather than assumed: `insert_edge`/`insert_node` are
/// rewritten to id-keyed WAL records (`rewrite_wal_dense`), and `edges_at`'s
/// id-keyed match arm compares directly against the live key with no
/// alias-walk in that direction, so today it answers with zero edges rather
/// than the wrong direction or partner. The assertion here is that it stays
/// that way — empty, not corrupted data — which is what would regress if a
/// future engine change made that direction resolve to *something* without
/// this tool's endpoint comparison being alias-safe.
#[test]
fn edges_at_after_a_rename_is_correct_by_current_key_and_empty_by_the_old_one() {
    let db = open("edges-at-alias");
    {
        let mut w = db.write();
        w.insert_node("Person", "old", vec![]).unwrap();
        w.insert_node("Person", "p1", vec![]).unwrap();
        w.insert_node("Person", "p2", vec![]).unwrap();
        w.insert_edge("Knows", "old", "p1").unwrap();
        w.insert_edge("Knows", "p2", "old").unwrap();
    }
    db.write().rename_node("old", "new").unwrap();
    let at = db.read().wal_total_commits().unwrap() - 1;

    // Queried by the current key: both edges, written under the old name,
    // show up with the right direction and partner.
    let text = task_reply(&one_task_call(
        db.clone(),
        "edges_at",
        json!({"key": "new", "at": at}),
    ));
    assert!(
        text.starts_with(&format!(
            "mushroomdb edges_at — new as of commit {at}: 2 edge(s)"
        )),
        "{text}"
    );
    assert!(text.contains("→ p1"), "{text}");
    assert!(text.contains("← p2"), "{text}");
    assert!(
        !text.contains("→ new") && !text.contains("← new"),
        "the node must not be reported as its own partner: {text}"
    );

    let report = task_report(db.clone(), "edges_at", json!({"key": "new", "at": at}));
    assert_eq!(report["key"], json!("new"));
    let edges = report["edges"].as_array().expect("edges array");
    assert_eq!(edges.len(), 2);
    for e in edges {
        assert!(
            e["src"] == json!("new") || e["dst"] == json!("new"),
            "every edge is incident to the current key: {e}"
        );
    }

    // Queried by the stale "old" key: empty — never the two edges shown
    // under a wrong direction or with the node named as its own partner.
    let text = task_reply(&one_task_call(
        db.clone(),
        "edges_at",
        json!({"key": "old", "at": at}),
    ));
    assert!(
        text.starts_with(&format!(
            "mushroomdb edges_at — old as of commit {at}: 0 edge(s)"
        )),
        "{text}"
    );

    let report = task_report(db, "edges_at", json!({"key": "old", "at": at}));
    assert_eq!(report["edges"], json!([]));
}

/// Binding: `edges_at` caps the edges listed per type at `limit` (default 10)
/// and counts the rest as "… and N more", the same contract `node_edges` has.
#[test]
fn edges_at_caps_each_type_and_counts_what_it_did_not_list() {
    let db = open("edges-at-limit");
    {
        let mut w = db.write();
        w.insert_node("Person", "hub", vec![]).unwrap();
        for i in 0..12 {
            let partner = format!("p{i:02}");
            w.insert_node("Person", &partner, vec![]).unwrap();
            w.insert_edge("Knows", "hub", &partner).unwrap();
        }
    }
    let at = db.read().wal_total_commits().unwrap() - 1;

    let text = task_reply(&one_task_call(
        db.clone(),
        "edges_at",
        json!({"key": "hub", "at": at}),
    ));
    assert!(
        text.starts_with(&format!(
            "mushroomdb edges_at — hub as of commit {at}: 12 edge(s)"
        )),
        "{text}"
    );
    assert!(text.contains("Knows (12)"), "{text}");
    assert_eq!(
        text.matches("→ p").count(),
        10,
        "default limit lists 10: {text}"
    );
    assert!(text.contains("… and 2 more"), "{text}");

    // A caller-specified limit is honored too — both under the default and
    // above it, which is what a caller reaching for the whole set asks for.
    let text = task_reply(&one_task_call(
        db.clone(),
        "edges_at",
        json!({"key": "hub", "at": at, "limit": 3}),
    ));
    assert_eq!(text.matches("→ p").count(), 3, "{text}");
    assert!(text.contains("… and 9 more"), "{text}");

    let text = task_reply(&one_task_call(
        db.clone(),
        "edges_at",
        json!({"key": "hub", "at": at, "limit": 12}),
    ));
    assert_eq!(
        text.matches("→ p").count(),
        12,
        "a limit above the default lists more: {text}"
    );
    assert!(!text.contains("… and"), "nothing was cut: {text}");

    // `json: true` lists what the text lists — at most `limit` per edge type —
    // and says how many there are, so no call silently returns every edge of
    // a hub node.
    let report = task_report(db.clone(), "edges_at", json!({"key": "hub", "at": at}));
    assert_eq!(
        report["edges"].as_array().map(Vec::len),
        Some(10),
        "{report}"
    );
    assert_eq!(report["listed"], json!(10));
    assert_eq!(report["total"], json!(12));

    let report = task_report(db, "edges_at", json!({"key": "hub", "at": at, "limit": 12}));
    assert_eq!(
        report["edges"].as_array().map(Vec::len),
        Some(12),
        "{report}"
    );
    assert_eq!(report["total"], json!(12));
}

/// Binding: `edges_at` answers the intersection question at a past commit —
/// the same keys-only shape `node_edges` gives, with the commit in the header
/// and in the report.
#[test]
fn edges_at_all_of_answers_with_partner_keys_at_a_past_commit() {
    let db = all_of_store("edges-at-all-of");
    let at = db.read().wal_total_commits().unwrap() - 1;

    let (text, report) = task_both(
        db.clone(),
        "edges_at",
        json!({"key": "hub", "at": at, "all_of": ["A", "B", "C"]}),
    );
    assert_eq!(
        text,
        format!(
            "mushroomdb edges_at — hub as of commit {at} — \
             partners linked by all of A, B, C: 2\no1, o2\n"
        ),
        "{text}"
    );
    assert_eq!(report["at"], json!(at));
    assert_eq!(report["partners"], json!(["o1", "o2"]));
    assert_eq!(report["total"], json!(2));

    let report = task_report(
        db.clone(),
        "edges_at",
        json!({"key": "hub", "at": at, "all_of": ["A", "B", "C"], "direction": "out"}),
    );
    assert_eq!(report["partners"], json!(["o1"]));

    // And one `edge_type` is the same compact listing for a single type.
    let (text, report) = task_both(
        db,
        "edges_at",
        json!({"key": "hub", "at": at, "edge_type": "A"}),
    );
    assert_eq!(
        text,
        format!("mushroomdb edges_at — hub as of commit {at}: 3 edge(s)\nA (3): o1, o2, o3\n"),
        "{text}"
    );
    assert_eq!(report["edge_type"], json!("A"));
    assert_eq!(report["edges"], json!(3));
    assert_eq!(report["partners"], json!(["o1", "o2", "o3"]));
    assert_eq!(report["rule"], json!(null), "a manual edge claims no rule");
}

// ── what_if ──────────────────────────────────────────────────────────────────

/// A memory store on a known directory, holding two `Org`s, one `Person` at
/// the first of them, and the rule that puts them together.
fn what_if_store(name: &str) -> (SharedDb, PathBuf) {
    let dir = tmp(name);
    let db = SharedDb::open(&dir).unwrap();
    {
        let mut w = db.write();
        for org in ["acme", "globex"] {
            w.insert_node("Org", org, vec![("id".into(), Value::Str(org.into()))])
                .unwrap();
        }
        w.create_rule(core_api::RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: core_api::Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
        w.insert_node(
            "Person",
            "p1",
            vec![
                ("id".into(), Value::Str("p1".into())),
                ("org_id".into(), Value::Str("acme".into())),
            ],
        )
        .unwrap();
    }
    (db, dir)
}

fn what_if_call(db: SharedDb, dir: &Path, args: Js) -> Js {
    let (res, out) = exchange_at(db, Some(dir.to_path_buf()), &call(1, "what_if", args));
    assert!(res.is_ok(), "{res:?}");
    parse_lines(&out).remove(0)
}

/// Binding: `what_if` names the edges a change would lose and gain, with the
/// rule behind each — computed by the engine directly, with nothing written
/// and nothing copied, so the live store is untouched afterwards.
#[test]
fn what_if_lists_the_edges_a_change_would_lose_and_gain() {
    let (db, dir) = what_if_store("what-if-flip");
    let commits_before = db.read().wal_total_commits().unwrap();

    let reply = what_if_call(
        db.clone(),
        &dir,
        json!({"key": "p1", "field": "org_id", "value": "globex"}),
    );
    let text = task_reply(&reply);
    assert_eq!(
        text,
        "mushroomdb what_if — p1.org_id = \"globex\": would lose 1, would gain 1\n\
         lost\n\
         \x20 WORKS_AT (1)\n\
         \x20   → acme  rule works_at\n\
         gained\n\
         \x20 WORKS_AT (1)\n\
         \x20   → globex  rule works_at\n",
        "{text}"
    );

    let reply = what_if_call(
        db.clone(),
        &dir,
        json!({"key": "p1", "field": "org_id", "value": "globex", "json": true}),
    );
    let report: Js = serde_json::from_str(
        reply["result"]["content"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
    assert_eq!(report["key"], json!("p1"));
    assert_eq!(report["field"], json!("org_id"));
    assert_eq!(report["value"], json!("globex"));
    assert_eq!(report["lost"][0]["edge_type"], json!("WORKS_AT"));
    assert_eq!(report["lost"][0]["src"], json!("p1"));
    assert_eq!(report["lost"][0]["dst"], json!("acme"));
    assert_eq!(report["lost"][0]["derived"], json!(true));
    assert_eq!(report["lost"][0]["rule"], json!("works_at"));
    assert_eq!(report["gained"][0]["dst"], json!("globex"));
    assert_eq!(report["gained"][0]["rule"], json!("works_at"));

    // Nothing was written: the store's own commit count and its live edges
    // are exactly what they were before the call.
    assert_eq!(db.read().wal_total_commits().unwrap(), commits_before);
    let live = task_report(db.clone(), "node_edges", json!({"key": "p1"}));
    assert_eq!(live["types"][0]["edges"][0]["other"], json!("acme"));

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binding: a change that moves nothing says so, rather than printing two
/// empty headings with no count.
#[test]
fn what_if_says_nothing_changes_when_nothing_does() {
    let (db, dir) = what_if_store("what-if-still");
    let reply = what_if_call(
        db.clone(),
        &dir,
        json!({"key": "p1", "field": "nickname", "value": "pip"}),
    );
    let text = task_reply(&reply);
    assert!(
        text.starts_with("mushroomdb what_if — p1.nickname = \"pip\": would lose 0, would gain 0"),
        "{text}"
    );
    assert_eq!(text.matches("  none\n").count(), 2, "{text}");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binding: the three ways to call `what_if` wrong are tool errors, the
/// unknown key is refused as a plain `KeyNotFound`, and no store path is
/// needed any more — the same call succeeds without one.
#[test]
fn what_if_refuses_an_unknown_key_and_an_unsupported_value() {
    let (db, dir) = what_if_store("what-if-bad");

    let reply = what_if_call(
        db.clone(),
        &dir,
        json!({"key": "ghost", "field": "org_id", "value": "globex"}),
    );
    assert!(error_text(&reply).contains("ghost"), "{reply}");

    let reply = what_if_call(
        db.clone(),
        &dir,
        // A list holding a null has no value the store can hold, which is the
        // one shape `json_to_value` refuses outright.
        json!({"key": "p1", "field": "org_id", "value": [null]}),
    );
    assert!(
        error_text(&reply).contains("not a supported value type"),
        "{reply}"
    );

    let reply = what_if_call(db.clone(), &dir, json!({"key": "p1", "field": "org_id"}));
    assert!(error_text(&reply).contains("missing value"), "{reply}");

    // There is no store to copy any more, so no store path is required: the
    // same call succeeds without one, unlike `sync`.
    let reply = one_task_call(
        db.clone(),
        "what_if",
        json!({"key": "p1", "field": "org_id", "value": "globex"}),
    );
    let text = task_reply(&reply);
    assert!(text.contains("would gain 1"), "{text}");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binding: a list-valued `value` — not just a scalar — is accepted, and the
/// reply is well-formed on both the text and `json: true` paths.
#[test]
fn what_if_accepts_a_list_valued_change() {
    let (db, dir) = what_if_store("what-if-list-value");

    let reply = what_if_call(
        db.clone(),
        &dir,
        json!({"key": "p1", "field": "specialties", "value": ["a", "b"]}),
    );
    let text = task_reply(&reply);
    assert!(
        text.starts_with(
            "mushroomdb what_if — p1.specialties = [\"a\",\"b\"]: would lose 0, would gain 0"
        ),
        "{text}"
    );
    assert_eq!(text.matches("  none\n").count(), 2, "{text}");

    let reply = what_if_call(
        db.clone(),
        &dir,
        json!({"key": "p1", "field": "specialties", "value": ["a", "b"], "json": true}),
    );
    let report: Js = serde_json::from_str(
        reply["result"]["content"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
    assert_eq!(report["key"], json!("p1"));
    assert_eq!(report["field"], json!("specialties"));
    assert_eq!(report["value"], json!(["a", "b"]));
    assert_eq!(report["lost"], json!([]));
    assert_eq!(report["gained"], json!([]));

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binding: `direction` narrows the grouped `edges_at` view the same way it
/// narrows `node_edges` — counts and listing both — rather than being parsed
/// and then ignored on the path that does not take `edge_type`/`all_of`.
#[test]
fn edges_at_grouped_view_honours_direction() {
    let db = all_of_store("edges-at-direction");
    let at = db.read().wal_total_commits().unwrap() - 1;

    // hub has seven edges at `at`: A and B to o1/o2/o3 outgoing, C to o1
    // outgoing, and C from o2 incoming.
    let text = task_reply(&one_task_call(
        db.clone(),
        "edges_at",
        json!({"key": "hub", "at": at}),
    ));
    assert!(text.contains(&format!("commit {at}: 8 edge(s)")), "{text}");

    let (text, report) = task_both(
        db.clone(),
        "edges_at",
        json!({"key": "hub", "at": at, "direction": "out"}),
    );
    assert!(
        text.contains(&format!("commit {at}: 7 edge(s)")),
        "the incoming C edge is out of the count: {text}"
    );
    assert!(text.contains("C (1)"), "{text}");
    assert!(!text.contains("← o2"), "{text}");
    assert_eq!(report["total"], json!(7), "{report}");

    let (text, report) = task_both(
        db,
        "edges_at",
        json!({"key": "hub", "at": at, "direction": "in"}),
    );
    assert!(text.contains(&format!("commit {at}: 1 edge(s)")), "{text}");
    assert!(text.contains("← o2"), "{text}");
    assert_eq!(report["total"], json!(1), "{report}");
    assert_eq!(
        report["edges"].as_array().map(Vec::len),
        Some(1),
        "{report}"
    );
}

/// Binding: `all_of` and `edge_type` together are an argument error, not a
/// silent win for one of them — they answer two different questions.
#[test]
fn all_of_and_edge_type_together_are_refused() {
    let db = all_of_store("edges-both-filters");
    let at = db.read().wal_total_commits().unwrap() - 1;

    let reply = one_task_call(
        db.clone(),
        "node_edges",
        json!({"key": "hub", "all_of": ["A"], "edge_type": "B"}),
    );
    assert!(
        error_text(&reply).contains("pass one of all_of or edge_type"),
        "{reply}"
    );

    let reply = one_task_call(
        db,
        "edges_at",
        json!({"key": "hub", "at": at, "all_of": ["A"], "edge_type": "B"}),
    );
    assert!(
        error_text(&reply).contains("pass one of all_of or edge_type"),
        "{reply}"
    );
}

/// Binding: the grouped `what_if` reply never loses a section to a line
/// budget. `limit` is the only cap, so fifty lost edges do not delete the
/// `gained` heading and the three edges under it.
#[test]
fn what_if_grouped_view_never_truncates_away_the_gained_section() {
    let db = open("what-if-no-truncation");
    {
        let mut w = db.write();
        for i in 0..50 {
            let k = format!("old{i:02}");
            w.insert_node(
                "Org",
                &k,
                vec![("sector".into(), Value::Str("tech".into()))],
            )
            .unwrap();
        }
        for i in 0..3 {
            let k = format!("new{i:02}");
            w.insert_node(
                "Org",
                &k,
                vec![("sector".into(), Value::Str("finance".into()))],
            )
            .unwrap();
        }
        w.create_rule(core_api::RuleDef {
            name: "any_org".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: core_api::Predicate::FieldEqual {
                field: "sector".into(),
            },
            edge_type: "IN_SECTOR".into(),
            weight_prop: None,
            max_edges: Some(1000),
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
        w.insert_node(
            "Person",
            "p1",
            vec![
                ("id".into(), Value::Str("p1".into())),
                ("sector".into(), Value::Str("tech".into())),
            ],
        )
        .unwrap();
    }

    let text = task_reply(&one_task_call(
        db,
        "what_if",
        json!({"key": "p1", "field": "sector", "value": "finance", "limit": 50}),
    ));
    assert!(text.contains("would lose 50, would gain 3"), "{text}");
    assert_eq!(
        text.matches("→ old").count(),
        50,
        "every lost edge the limit allows is listed: {text}"
    );
    assert!(
        text.contains("gained\n  IN_SECTOR (3)\n"),
        "the gained section survives a full lost section: {text}"
    );
    for i in 0..3 {
        assert!(text.contains(&format!("→ new{i:02}")), "{text}");
    }
    assert!(!text.contains("… and"), "nothing was cut at all: {text}");
}

/// Binding: `what_if` with an `edge_type` answers about that type alone —
/// counts included — as two lists of partner keys with the rule named once,
/// and `limit` cuts each list and says what it cut.
#[test]
fn what_if_with_an_edge_type_answers_in_partner_keys() {
    let db = wide_store("what-if-keys", 25);

    let text = task_reply(&one_task_call(
        db.clone(),
        "what_if",
        json!({"key": "p1", "field": "sector", "value": "finance", "edge_type": "IN_SECTOR", "limit": 3}),
    ));
    assert!(
        text.starts_with(
            "mushroomdb what_if — p1.sector = \"finance\": would lose 25, would gain 0\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("lost\nIN_SECTOR (25, rule any_org): org00, org01, org02\n"),
        "{text}"
    );
    assert!(text.contains("… and 22 more\n"), "{text}");
    assert!(text.contains("gained\n  none\n"), "{text}");

    // A type the change does not touch is a reply about that type: zero, zero.
    let text = task_reply(&one_task_call(
        db.clone(),
        "what_if",
        json!({"key": "p1", "field": "sector", "value": "finance", "edge_type": "NOPE"}),
    ));
    assert!(text.contains("would lose 0, would gain 0"), "{text}");
    assert_eq!(text.matches("  none\n").count(), 2, "{text}");

    // `json: true` obeys the same limit, and still says how many there are.
    let report = task_report(
        db.clone(),
        "what_if",
        json!({"key": "p1", "field": "sector", "value": "finance", "edge_type": "IN_SECTOR", "limit": 2}),
    );
    assert_eq!(report["edge_type"], json!("IN_SECTOR"));
    assert_eq!(report["lost"].as_array().map(Vec::len), Some(2), "{report}");
    assert_eq!(report["lost_total"], json!(25));
    assert_eq!(report["gained"], json!([]));
    assert_eq!(report["gained_total"], json!(0));

    // Without an `edge_type` the grouped view is still the reply, and the
    // default limit of ten now caps it instead of printing all twenty-five.
    let text = task_reply(&one_task_call(
        db.clone(),
        "what_if",
        json!({"key": "p1", "field": "sector", "value": "finance"}),
    ));
    assert_eq!(
        text.matches("→ org").count(),
        10,
        "ten listed by default: {text}"
    );
    assert!(text.contains("    … and 15 more\n"), "{text}");

    let report = task_report(
        db,
        "what_if",
        json!({"key": "p1", "field": "sector", "value": "finance"}),
    );
    assert_eq!(
        report["lost"].as_array().map(Vec::len),
        Some(10),
        "{report}"
    );
    assert_eq!(report["lost_total"], json!(25));
}

// ── descriptions ─────────────────────────────────────────────────────────────

/// Binding: every tool a memory store advertises opens its description with
/// the question it answers.
///
/// The listing is what a host ranks tools by, and the first association run
/// paid thirty-four `ToolSearch` turns discovering names whose descriptions
/// said what they *returned* rather than what they were *for*.
#[test]
fn every_association_tool_description_opens_with_its_question() {
    const OPENERS: [(&str, &str); 15] = [
        ("query", "Who may see this"),
        ("explain_association", "Why are A and B related"),
        ("neighborhood", "What is around K"),
        ("node_info", "What is K —"),
        ("node_edges", "What is K related to"),
        ("was_linked", "Were A and B linked at commit C"),
        (
            "edges_at",
            "What did K's relationships look like at commit C",
        ),
        ("what_if", "What changes if K's FIELD became VALUE"),
        ("node_history", "What has happened to K"),
        ("edge_history", "When did A and B become linked"),
        ("find_similar", "What is most like this"),
        ("hybrid_search", "What matches these words and this vector"),
        ("remember", "Remember this for next time"),
        ("recall", "What do I already know about this"),
        ("stats", "How big is this store"),
    ];

    let (res, out) = exchange(open("descriptions"), &req(json!(1), "tools/list", None));
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let tools = replies[0]["result"]["tools"].as_array().expect("tools");
    let described = |name: &str| -> String {
        let d = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed"))["description"]
            .as_str()
            .expect("description")
            .to_string();
        // The graph tools carry the ranking prefix; the question is what
        // follows it.
        d.strip_prefix("Advanced: ").unwrap_or(&d).to_string()
    };

    for (name, opener) in OPENERS {
        let d = described(name);
        assert!(
            d.starts_with(opener),
            "{name} must open with the question it answers, got {d:?}"
        );
    }
    assert_eq!(
        OPENERS.map(|(n, _)| n).to_vec(),
        ASSOCIATION_TOOLS.to_vec(),
        "the openers cover the whole surface, in its order"
    );

    // `query` is the one tool whose argument a session has to be told about:
    // the dialect it speaks and the restriction it can answer under.
    let q = described("query");
    assert!(q.contains("Cypher"), "{q}");
    assert!(q.contains("'role'"), "{q}");
    assert!(q.contains("'as_of'"), "{q}");
}

/// Binding: a key the graph does not hold is a tool error that names the key,
/// not an empty answer.
///
/// `explain` resolves both keys to dense ids before it looks at an edge, so a
/// typo cannot come back as "these two are unrelated" — which is the one wrong
/// answer this tool could give.
#[test]
fn explain_association_on_an_unknown_key_names_it() {
    let db = association_store("explain-unknown");
    let err = error_text(&one_task_call(
        db.clone(),
        "explain_association",
        json!({"a": "p1", "b": "ghost"}),
    ));
    assert!(err.contains("ghost"), "{err}");
    assert!(
        !err.contains("relationship(s)"),
        "an unknown key is an error, not a digest: {err}"
    );

    let missing_arg = error_text(&one_task_call(
        db,
        "explain_association",
        json!({"a": "p1"}),
    ));
    assert!(missing_arg.contains("missing b"), "{missing_arg}");
}

/// A memory store with two labels and a `client` role that may see only one of
/// them.
fn roles_store(name: &str) -> SharedDb {
    let db = open(name);
    {
        let mut w = db.write();
        for key in ["company-1", "company-2"] {
            w.insert_node("Company", key, vec![("id".into(), Value::Str(key.into()))])
                .unwrap();
        }
        for key in ["talent-1", "talent-2"] {
            w.insert_node("Talent", key, vec![("id".into(), Value::Str(key.into()))])
                .unwrap();
        }
        w.apply_schema(&core_api::Schema {
            roles: vec![core_api::RoleDef {
                name: "client".into(),
                keys: vec![],
                labels: vec!["Company".into()],
                visible_where: None,
                write: None,
            }],
            ..Default::default()
        })
        .unwrap();
    }
    db
}

/// Binding: `query` with a `role` answers as that role — only the nodes the
/// store's `roles.json` lets it see — and says so plainly when the role is not
/// one of them or when the caller passed a mask as well.
#[test]
fn query_with_a_role_sees_only_that_roles_labels() {
    let db = roles_store("query-role");
    let rows = content_json(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n) RETURN n.id ORDER BY n.id", "role": "client"}),
    ));
    assert_eq!(
        rows["rows"],
        json!([["company-1"], ["company-2"]]),
        "a role sees its own labels and nothing else"
    );

    let unmasked = content_json(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n) RETURN n.id ORDER BY n.id"}),
    ));
    assert_eq!(
        unmasked["rows"].as_array().map(Vec::len),
        Some(4),
        "and the same query without a role still sees everything"
    );

    let unknown = error_text(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n) RETURN n", "role": "nobody"}),
    ));
    assert!(unknown.contains("unknown role 'nobody'"), "{unknown}");

    let both = error_text(&one_task_call(
        db,
        "query",
        json!({"cypher": "MATCH (n) RETURN n", "role": "client", "mask": ["company-1"]}),
    ));
    assert!(
        both.contains("role") && both.contains("mask"),
        "one restriction or the other, never both: {both}"
    );
}

/// Binding: `query` takes `as_of` — a past commit — and it composes with
/// `role` and with `mask`.  Both restrictions are resolved against the graph
/// as it was at that commit, and a write at a past commit is a tool error.
#[test]
fn query_as_of_composes_with_a_role_and_a_mask() {
    let db = open("query-asof-role");
    let at_one = {
        let mut w = db.write();
        w.insert_node("Public", "p1", vec![]).unwrap();
        let at = w.wal_total_commits().unwrap() - 1;
        w.insert_node("Public", "p2", vec![]).unwrap();
        w.insert_node("Secret", "s1", vec![]).unwrap();
        w.apply_schema(&core_api::Schema {
            roles: vec![core_api::RoleDef {
                name: "reader".into(),
                keys: vec![],
                labels: vec!["Public".into()],
                visible_where: None,
                write: None,
            }],
            ..Default::default()
        })
        .unwrap();
        at
    };

    // A role at a past commit sees only what it could see then.
    let then = content_json(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n) RETURN n", "role": "reader", "as_of": at_one}),
    ));
    assert_eq!(then["rows"], json!([["p1"]]));

    // The same role now sees both Public nodes and never the Secret one.
    let now = content_json(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n) RETURN n", "role": "reader"}),
    ));
    assert_eq!(now["rows"], json!([["p1"], ["p2"]]));

    // `as_of` alone time-travels, unrestricted.
    let plain = content_json(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n) RETURN n", "as_of": at_one}),
    ));
    assert_eq!(plain["rows"], json!([["p1"]]));

    // `as_of` with a mask resolves the allow-list against the as-of graph.
    let masked = content_json(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n) RETURN n", "mask": ["p1", "p2"], "as_of": at_one}),
    ));
    assert_eq!(masked["rows"], json!([["p1"]]));

    // A write at a past commit is a tool error, not a write.
    let write = error_text(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "CREATE (x:Public {id:'z'})", "as_of": at_one}),
    ));
    assert!(write.contains("read-only"), "{write}");
    assert!(!db.read().has_node("z"), "the write must not have landed");

    // A non-integer `as_of` names what it wants.
    let bad = error_text(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n) RETURN n", "as_of": "yesterday"}),
    ));
    assert!(bad.contains("as_of"), "{bad}");

    // An out-of-range commit carries the retained range.
    let far = error_text(&one_task_call(
        db,
        "query",
        json!({"cypher": "MATCH (n) RETURN n", "role": "reader", "as_of": 9_999}),
    ));
    assert!(far.contains("out of range"), "{far}");
}

/// Binding: a store carrying the `GitSync` marker — a repository was ingested
/// into it — lists three tools and nothing else. A host defers MCP schemas and
/// makes the model search for them, so what is listed is what gets found.
#[test]
fn a_code_graph_store_lists_three_tools_by_default() {
    let (res, out) = exchange(
        code_store("surface-code"),
        &req(json!(1), "tools/list", None),
    );
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let names: Vec<&str> = replies[0]["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["explore", "query", "stats"]);
}

/// Binding: the surface decides what is *listed*, never what is served. Every
/// tool the memory surface advertises is still callable on a code-graph store,
/// and `explore` is still callable on a memory store.
#[test]
fn a_hidden_tool_is_still_callable_on_either_surface() {
    let map = task_reply(&one_task_call(
        code_store("surface-hidden"),
        "map",
        json!({}),
    ));
    assert!(
        map.starts_with("mushroomdb map —"),
        "map is unlisted on a code graph but must still answer: {map}"
    );
    let explore = task_reply(&one_task_call(
        open("surface-memory-explore"),
        "explore",
        json!({"target": "x"}),
    ));
    assert!(
        explore.contains("unknown: x"),
        "explore is unlisted on a memory store but must still answer: {explore}"
    );
}

/// Binding: an unlisted tool is still served. The flag decides what is
/// advertised, not what a caller that knows the name can reach.
#[test]
fn an_unlisted_graph_tool_is_still_callable() {
    let (res, out) = exchange(
        code_store("list-unlisted"),
        &call(1, "node_info", json!({"key": "src/core.rs"})),
    );
    assert!(res.is_ok(), "{res:?}");
    let reply = parse_lines(&out).remove(0);
    assert!(
        !reply["result"]["isError"].as_bool().unwrap_or(false),
        "node_info is unlisted by default but must still answer: {reply}"
    );
}

/// Binding: `--all-tools` lists 27, task tools first in their fixed order, and
/// every one of the thirteen graph tools carries the `Advanced:` prefix.
#[test]
fn tools_list_has_27_tools_task_tools_first_and_advanced_prefix() {
    let (res, out) = exchange_all_tools(open("list-order"), &req(json!(1), "tools/list", None));
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let tools = replies[0]["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();

    let expected: Vec<&str> = TASK_TOOLS
        .iter()
        .chain(ADVANCED_TOOLS.iter())
        .copied()
        .collect();
    assert_eq!(names, expected, "tools/list order");
    assert_eq!(tools.len(), 27);

    for t in tools.iter().take(TASK_TOOLS.len()) {
        let d = t["description"].as_str().expect("description");
        assert!(
            !d.starts_with("Advanced:"),
            "task tool {} must not be prefixed: {d}",
            t["name"]
        );
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
    }
    for t in tools.iter().skip(TASK_TOOLS.len()) {
        let d = t["description"].as_str().expect("description");
        assert!(
            d.starts_with("Advanced: "),
            "{} must be prefixed Advanced: got {d}",
            t["name"]
        );
    }

    // The schemas the plan fixes. `json` is the one argument they all share:
    // it is what a program asks for the report with, now that no reply carries
    // one by default.
    let by_name = |n: &str| tools.iter().find(|t| t["name"] == n).expect("tool");
    for t in TASK_TOOLS {
        assert_eq!(
            by_name(t)["inputSchema"]["properties"]["json"]["type"],
            json!("boolean"),
            "{t} must offer the json argument"
        );
    }
    for t in ADVANCED_TOOLS {
        assert!(
            by_name(t)["inputSchema"]["properties"]
                .get("json")
                .is_none(),
            "{t} is not a task tool and must not offer json"
        );
    }
    assert_eq!(
        by_name("map")["inputSchema"]["properties"]
            .as_object()
            .map(serde_json::Map::len),
        Some(1),
        "map takes nothing but json"
    );
    assert_eq!(
        by_name("sync")["inputSchema"]["properties"]
            .as_object()
            .map(serde_json::Map::len),
        Some(1),
        "sync takes nothing but json"
    );
    assert_eq!(
        by_name("context")["inputSchema"]["required"],
        json!(["target"])
    );
    assert_eq!(
        by_name("explore")["inputSchema"]["required"],
        json!(["target"])
    );
    assert_eq!(
        by_name("explore")["inputSchema"]["properties"]["depth"]["enum"],
        json!(["context", "impact", "history", "all"])
    );
    assert_eq!(
        by_name("owners")["inputSchema"]["required"],
        json!(["path"])
    );
    assert_eq!(by_name("why")["inputSchema"]["required"], json!(["a", "b"]));
    assert_eq!(
        by_name("recall")["inputSchema"]["required"],
        json!(["topic"])
    );
    assert_eq!(
        by_name("remember")["inputSchema"]["required"],
        json!(["text"])
    );
    assert!(by_name("impact")["inputSchema"].get("required").is_none());
    assert_eq!(
        by_name("remember")["inputSchema"]["properties"]["kind"]["enum"],
        json!(["note", "decision", "todo"])
    );
}

/// Binding: the published server card lists exactly the tools the server
/// serves, in the same order.
///
/// The card is the whole surface, not the default listing: it says what
/// `mushroomdb mcp --all-tools` advertises and what every name in it can be
/// called as, so it is compared against that list rather than the thirteen a
/// default session sees.
#[test]
fn server_card_lists_the_same_tools_in_the_same_order() {
    let card_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.well-known/mcp/server-card.json");
    let card: Js = serde_json::from_str(&std::fs::read_to_string(&card_path).expect("server card"))
        .expect("server card json");
    let listed: Vec<&str> = card["tools"]
        .as_array()
        .expect("card tools array")
        .iter()
        .map(|t| t.as_str().expect("card tool name"))
        .collect();

    let (res, out) = exchange_all_tools(open("card-drift"), &req(json!(1), "tools/list", None));
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let served: Vec<&str> = replies[0]["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();

    assert_eq!(listed, served, "{} is out of date", card_path.display());
}

/// Binding: `map` on a store with nothing in it names the command that fills it.
#[test]
fn map_on_empty_store_is_helpful() {
    let (text, structured) = task_both(open("map-empty"), "map", json!({}));
    assert!(
        text.contains("empty store") && text.contains("ingest-git"),
        "empty map must say what to run: {text}"
    );
    assert_eq!(structured["files"], json!(0));
    assert_eq!(structured["symbols"], json!(0));
}

/// Binding: `map` counts what the graph holds and renders the same numbers.
#[test]
fn map_reports_the_graphed_repository() {
    let (text, structured) = task_both(code_store("map-full"), "map", json!({}));
    assert_eq!(structured["files"], json!(3));
    assert_eq!(structured["symbols"], json!(2));
    assert_eq!(structured["commits"], json!(4));
    assert_eq!(structured["authors"], json!(2));
    assert!(text.starts_with("mushroomdb map — 3 files"), "{text}");
    assert!(
        text.lines().count() <= 40,
        "map must stay within its line budget: {text}"
    );
}

/// Binding: a `map` served from a handle another handle wrote through is
/// current without the server being restarted.
#[test]
fn map_reflects_writes_made_by_another_handle() {
    let dir = tmp("map-follows");
    let db = SharedDb::open(&dir).expect("open");
    seed_code_graph(&db);

    let before = task_report(db.clone(), "map", json!({}));
    assert_eq!(before["files"], json!(3));

    // A second handle on the same directory — what a git hook is — inserts a
    // fourth file and exits, releasing the store's write lock.
    {
        let mut other = core_api::GraphDb::open(&dir).expect("second handle");
        other
            .insert_node(
                "File",
                "src/extra.rs",
                vec![
                    ("id".into(), s("src/extra.rs")),
                    ("path".into(), s("src/extra.rs")),
                    ("dir".into(), s("src")),
                    ("lang".into(), s("rust")),
                    ("lines".into(), Value::Int(9)),
                ],
            )
            .expect("insert through second handle");
    }
    // No wait: the read path checks the store on every read, so the very next
    // call sees the other handle's commit.
    let (text, after) = task_both(db.clone(), "map", json!({}));
    assert_eq!(
        after["files"],
        json!(4),
        "the server must follow the other handle's write: {text}"
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binding: `context` on a bare symbol name resolves it and reports its file,
/// its signature and what calls it.
#[test]
fn context_on_symbol() {
    let (text, structured) = task_both(
        code_store("context-symbol"),
        "context",
        json!({"target": "core::init"}),
    );
    assert_eq!(
        structured["target"]["symbol"]["key"],
        json!("src/core.rs#core::init")
    );
    assert_eq!(structured["file"], json!("src/core.rs"));
    assert_eq!(structured["signature"], json!("fn core::init()"));
    // Callers come back as call sites grouped by the file they sit in.
    let callers: Vec<(&str, Vec<&str>)> = structured["callers"]
        .as_array()
        .expect("callers")
        .iter()
        .map(|c| {
            (
                c["file"].as_str().expect("caller file"),
                c["symbols"]
                    .as_array()
                    .expect("caller symbols")
                    .iter()
                    .map(|s| s.as_str().expect("caller key"))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(callers, vec![("src/web.rs", vec!["src/web.rs#web::serve"])]);
    assert!(text.contains("core::init"), "{text}");
    assert!(
        text.lines().count() <= 60,
        "context must stay within its line budget: {text}"
    );
}

/// Binding: a default `context` answers with a pointer and the graph's facts,
/// inside the reply budget; `full: true` quotes the body from the working tree.
#[test]
fn context_answers_with_pointers_by_default_and_bodies_on_full() {
    let db = code_store("context-pointers");
    // The seed's marker names a working tree that is not there, so `full`
    // would have nothing to quote. Point it at a real one.
    let repo = tmp("context-pointers-tree");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    let body: String = (1..=30).map(|n| format!("// line {n}\n")).collect();
    std::fs::write(repo.join("src/core.rs"), body).expect("write source");
    db.write()
        .set_prop(
            "__mushroomdb_git_sync__",
            "repo",
            s(&repo.to_string_lossy()),
        )
        .expect("repo");

    let (text, structured) = task_both(db.clone(), "context", json!({"target": "core::init"}));
    assert!(
        structured["source"].is_null(),
        "no body by default: {structured}"
    );
    assert!(
        text.contains("src/core.rs:10-20"),
        "pointer line present: {text}"
    );
    assert!(!text.contains("// line 10"), "no body by default: {text}");
    assert!(
        text.len() <= 4_800,
        "default context reply within budget: {}",
        text.len()
    );

    let (full, structured_full) =
        task_both(db, "context", json!({"target": "core::init", "full": true}));
    assert!(
        structured_full["source"].is_string(),
        "full quotes the working tree: {structured_full}"
    );
    assert!(full.contains("// line 10"), "the body is quoted: {full}");
    assert!(
        full.len() > text.len(),
        "the default is the shorter answer: {} vs {}",
        text.len(),
        full.len()
    );
}

/// Binding: `context` on a target the graph does not know says so rather than
/// failing.
#[test]
fn context_on_unknown_target_is_not_an_error() {
    let (text, structured) = task_both(
        code_store("context-unknown"),
        "context",
        json!({"target": "nope"}),
    );
    assert_eq!(structured["target"]["unknown"]["target"], json!("nope"));
    assert!(!text.is_empty());
}

/// Binding: `context` without a target is a tool error.
#[test]
fn context_without_target_is_a_tool_error() {
    let reply = one_task_call(code_store("context-no-target"), "context", json!({}));
    assert!(error_text(&reply).contains("target"));
}

/// Binding: `explore` at `all` is one call for the three answers — the
/// context, the blast radius, and who owns it — inside the default budget.
#[test]
fn explore_all_composes_context_impact_and_history_within_budget() {
    let (text, structured) = task_both(
        code_store("explore-all"),
        "explore",
        json!({"target": "core::init", "depth": "all"}),
    );
    assert_eq!(
        structured["context"]["target"]["symbol"]["key"],
        json!("src/core.rs#core::init")
    );
    assert_eq!(structured["depth"], json!("all"));
    assert!(
        structured["impact"].is_object() && structured["owners"].is_object(),
        "all carries the blast radius and the ownership: {structured}"
    );
    assert!(text.len() <= 4_800, "{} bytes:\n{text}", text.len());
    for want in ["callers", "impact:", "owner:"] {
        assert!(text.contains(want), "the digest is missing {want}:\n{text}");
    }
    // The partners reach a caller through the report; the digest names them
    // once, on the context section's own `co-change` line.
    assert!(
        structured["partners"].is_array(),
        "the report carries the co-change partners: {structured}"
    );
    assert!(
        !text.contains("changes with"),
        "and the digest does not repeat them:\n{text}"
    );
}

/// Binding: the default depth is `context`, and it costs neither the blast
/// radius nor the history.
#[test]
fn explore_defaults_to_context_depth() {
    let (text, structured) = task_both(
        code_store("explore-default"),
        "explore",
        json!({"target": "core::init"}),
    );
    assert_eq!(structured["depth"], json!("context"));
    assert!(structured["impact"].is_null() && structured["owners"].is_null());
    assert!(!text.contains("impact:"), "{text}");
}

/// Binding: `budget` is in tokens and caps the whole reply, framing included.
#[test]
fn explore_budget_caps_the_reply() {
    let reply = one_task_call(
        code_store("explore-budget"),
        "explore",
        json!({"target": "core::init", "depth": "all", "budget": 200}),
    );
    let text = task_reply(&reply);
    assert!(
        text.len() <= 200 * 4,
        "a 200-token budget is 800 bytes, got {}:\n{text}",
        text.len()
    );
    assert!(!text.is_empty(), "the header survives any budget");
}

/// Binding: a depth that is not one of the four is a tool error naming them.
#[test]
fn explore_with_an_unknown_depth_is_a_tool_error() {
    let reply = one_task_call(
        code_store("explore-depth"),
        "explore",
        json!({"target": "core::init", "depth": "everything"}),
    );
    let msg = error_text(&reply);
    assert!(msg.contains("depth") && msg.contains("history"), "{msg}");
}

/// Binding: `impact` on an explicit file list marks the partners that are
/// themselves in that list.
#[test]
fn impact_explicit_files_marks_modified() {
    let (text, structured) = task_both(
        code_store("impact-explicit"),
        "impact",
        json!({"files": ["src/core.rs", "src/web.rs"]}),
    );
    let files = structured["files"].as_array().expect("files");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], json!("src/core.rs"));

    let partners = files[0]["partners"].as_array().expect("partners");
    let web = partners
        .iter()
        .find(|p| p["path"] == "src/web.rs")
        .expect("src/web.rs is a co-change partner of src/core.rs");
    assert_eq!(web["modified"], json!(true), "it is in the changed set");
    let util = partners.iter().find(|p| p["path"] == "src/util.rs");
    if let Some(util) = util {
        assert_eq!(
            util["modified"],
            json!(false),
            "it is not in the changed set"
        );
    }
    assert!(text.contains("src/web.rs 1.00 modified"), "{text}");
    assert!(
        text.lines().count() <= 25,
        "impact must stay within its line budget: {text}"
    );
}

/// Binding: `impact` reports a path the graph has never seen as unknown.
#[test]
fn impact_reports_unknown_paths() {
    let (text, structured) = task_both(
        code_store("impact-unknown"),
        "impact",
        json!({"files": ["src/core.rs", "no/such.rs"]}),
    );
    assert_eq!(structured["unknown"], json!(["no/such.rs"]));
    assert!(text.contains("unknown: no/such.rs"), "{text}");
}

// The default `impact` file list — where the diff comes from, how it is
// filtered, and what happens with no checkout — is decided before the graph is
// touched, and is covered by the unit tests in `crates/server/src/mcp_tasks.rs`.
// They take `$CLAUDE_PROJECT_DIR` as an argument; asserting it here would mean
// setting a process-global variable in a binary whose other tests read the
// environment concurrently.

/// Binding: `owners` names the top author once, with the key in parentheses.
#[test]
fn owners_reports_the_top_author() {
    let (text, structured) = task_both(
        code_store("owners-ok"),
        "owners",
        json!({"path": "src/core.rs"}),
    );
    assert_eq!(structured["path"], json!("src/core.rs"));
    assert!(text.contains("Ada Example (a@example.test)"), "{text}");
    assert!(
        text.lines().count() <= 25,
        "owners must stay within its line budget: {text}"
    );
}

/// Binding: `owners` on a path the store holds no file for is a tool error.
#[test]
fn owners_unknown_path_error() {
    let reply = one_task_call(
        code_store("owners-unknown"),
        "owners",
        json!({"path": "no/such.rs"}),
    );
    let msg = error_text(&reply);
    assert!(msg.contains("no/such.rs"), "{msg}");
}

/// Binding: `why` names both unknown keys rather than only the first.
#[test]
fn why_unknown_keys_say_unknown() {
    let (text, structured) = task_both(
        code_store("why-unknown"),
        "why",
        json!({"a": "nope", "b": "zzz"}),
    );
    assert!(text.contains("unknown:"), "{text}");
    assert_eq!(structured["unknown"], json!(["nope", "zzz"]));
}

/// Binding: `why` between two co-changed files reports the link and its
/// evidence.
#[test]
fn why_reports_the_link_between_two_files() {
    let (text, structured) = task_both(
        code_store("why-link"),
        "why",
        json!({"a": "src/core.rs", "b": "src/web.rs"}),
    );
    let links = structured["links"].as_array().expect("links");
    assert!(
        links.iter().any(|l| l["edge_type"] == "CO_CHANGED"),
        "expected a CO_CHANGED link: {structured}"
    );
    assert!(text.contains("CO_CHANGED"), "{text}");
    assert!(
        text.lines().count() <= 25,
        "why must stay within its line budget: {text}"
    );
}

/// Binding: `recall` turns a topic that names something into a digest of
/// pointers to the nodes nearest it.
#[test]
fn recall_returns_digest() {
    let (text, structured) = task_both(
        code_store("recall-topic"),
        "recall",
        json!({"topic": "src/core.rs"}),
    );
    assert_eq!(structured["topic"], json!("src/core.rs"));
    assert!(
        text.contains("src/core.rs"),
        "the digest must name the matching node: {text}"
    );
    // `text` here is the digest with its framing line stripped by `task_reply`,
    // so putting it back must give exactly what core-api produced.
    assert_eq!(
        structured["digest"],
        json!(format!("{UNTRUSTED_FRAMING}{text}")),
        "the reply shows the digest unaltered"
    );
}

/// Binding: a topic with nothing searchable in it is answered, not an error.
#[test]
fn recall_on_an_unsearchable_topic_says_nothing_matched() {
    let text = task_reply(&one_task_call(
        code_store("recall-empty"),
        "recall",
        json!({"topic": "!!! ???"}),
    ));
    assert!(text.contains("nothing"), "{text}");
}

/// Binding: `remember` writes a note and returns its key; unknown `about`
/// keys are all named, and nothing is written.
#[test]
fn remember_writes_note_and_rejects_unknown_about() {
    let db = code_store("remember");

    let reply = one_task_call(
        db.clone(),
        "remember",
        json!({"text": "core::init is the entry point", "about": ["src/core.rs"], "kind": "decision"}),
    );
    let text = task_reply(&reply);
    // The digest is the only place the key is now reported, so it has to name
    // it in full: one call, one note, and the caller can cite what it reads.
    let key = text
        .split_whitespace()
        .find(|w| w.starts_with("note:"))
        .unwrap_or_else(|| panic!("the render must name the note key: {text}"))
        .to_string();
    assert_eq!(key.len(), "note:".len() + 16, "{key}");
    assert!(db.read().has_node(&key), "the note must be in the store");

    // Two unknown keys: both are named, sorted, and nothing is written.
    let before = db.read().node_count();
    let reply = one_task_call(
        db.clone(),
        "remember",
        json!({"text": "about nothing that exists", "about": ["zzz.rs", "no/such.rs"]}),
    );
    let msg = error_text(&reply);
    assert!(
        msg.contains("no/such.rs") && msg.contains("zzz.rs"),
        "{msg}"
    );
    assert_eq!(db.read().node_count(), before, "nothing may be written");
}

/// Binding: `remember` needs text.
#[test]
fn remember_without_text_is_a_tool_error() {
    let reply = one_task_call(code_store("remember-no-text"), "remember", json!({}));
    assert!(error_text(&reply).contains("text"));
}

/// Binding: `remember` rejects a `kind` outside the enum.
#[test]
fn remember_rejects_an_unknown_kind() {
    let reply = one_task_call(
        code_store("remember-kind"),
        "remember",
        json!({"text": "hello", "kind": "shopping list"}),
    );
    assert!(error_text(&reply).contains("kind"));
}

/// Binding: `sync` cannot run without knowing where the store is, and says so.
#[test]
fn sync_without_db_dir_is_a_tool_error() {
    let reply = one_task_call(code_store("sync-no-dir"), "sync", json!({}));
    let msg = error_text(&reply);
    assert!(msg.contains("store path unknown"), "{msg}");
}

/// Binding: with a store path, `sync` runs this binary and reports what it
/// could not do rather than panicking.
#[test]
fn sync_with_db_dir_reports_the_child_failure() {
    let dir = tmp("sync-with-dir");
    let db = SharedDb::open(&dir).expect("open");
    let (res, out) = exchange_at(db.clone(), Some(dir.clone()), &call(1, "sync", json!({})));
    assert!(res.is_ok(), "{res:?}");
    let reply = parse_lines(&out).remove(0);
    // `current_exe()` under `cargo test` is this test binary, not the CLI, so
    // the run cannot produce a sync report. What matters is that the tool
    // reports that as an error instead of hanging or panicking.
    let msg = error_text(&reply);
    assert!(msg.contains("sync"), "{msg}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binding: every task tool stamps its text with the untrusted-data framing
/// line, exactly once, before any repository content.
///
/// `task_reply` asserts this on each tool's own test too; this one sweeps the
/// whole list in one place so another tool cannot be added without a framed
/// answer.
#[test]
fn every_task_tool_frames_its_text_as_untrusted() {
    let db = code_store("framing");
    let args = |tool: &str| match tool {
        "explore" | "context" => json!({"target": "core::init"}),
        "impact" => json!({"files": ["src/core.rs"]}),
        "owners" => json!({"path": "src/core.rs"}),
        "why" | "explain_association" => json!({"a": "src/core.rs", "b": "src/web.rs"}),
        "recall" => json!({"topic": "src/core.rs"}),
        "remember" => json!({"text": "framing check", "about": ["src/core.rs"]}),
        "node_edges" | "neighborhood" => json!({"key": "src/core.rs"}),
        "edges_at" => json!({"key": "src/core.rs", "at": 0}),
        "what_if" => json!({"key": "src/core.rs", "field": "lines", "value": 2}),
        _ => json!({}),
    };
    for tool in TASK_TOOLS {
        // `sync` is the one tool left that answers with an error here: it
        // needs the store path this transcript does not pass. A tool error is
        // a message to the caller, not graph content, and carries no framing
        // by design.
        if tool == "sync" {
            let reply = one_task_call(db.clone(), tool, args(tool));
            assert!(
                !error_text(&reply).starts_with(UNTRUSTED_FRAMING),
                "a tool error is not graph content"
            );
            continue;
        }
        let reply = one_task_call(db.clone(), tool, args(tool));
        let full = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: content[0].text"));
        assert!(
            full.starts_with(UNTRUSTED_FRAMING),
            "{tool} must open with the framing line, got {full:?}"
        );
        assert_eq!(
            full.matches(UNTRUSTED_FRAMING).count(),
            1,
            "{tool} must carry the framing line exactly once"
        );
        // And the report is not shipped beside it.
        assert!(
            reply["result"].get("structuredContent").is_none(),
            "{tool} must answer with text alone"
        );
    }
}

/// Binding: `json: true` swaps the digest for the report — the text content is
/// the serialised report, there is still no `structuredContent`, and the two
/// modes describe the same call.
#[test]
fn json_true_answers_with_the_report_as_the_text() {
    let db = code_store("json-arg");
    let (digest, report) = task_both(db, "impact", json!({"files": ["src/core.rs"]}));
    assert!(digest.starts_with("mushroomdb impact"), "{digest}");
    assert_eq!(report["files"][0]["path"], json!("src/core.rs"));
    assert!(
        report.get("text").is_none(),
        "the report must not carry a copy of the digest: {report}"
    );
}

/// Binding: a `json` reply carries no framing line — it is a document to parse,
/// not prose to read — but every string in it is still sanitized, because the
/// graph content in it came from the repository just as the digest's did.
///
/// `serde_json` escaping keeps a control character from breaking the document.
/// It does nothing about what the reader sees after parsing, which is where an
/// escape sequence, a carriage return or a `DEL` would land.
#[test]
fn a_json_reply_is_unframed_and_sanitized() {
    // The reports sanitize the graph content they are built from, so the field
    // that proves this pass runs is one they do not touch: `recall` echoes the
    // caller's own topic back verbatim.
    let hostile = "core \u{1b}[31m\rforged\u{7f} and\ttabbed";
    let db = code_store("json-sanitize");
    let reply = one_task_call(
        db.clone(),
        "recall",
        json!({"topic": hostile, "json": true}),
    );
    let raw = task_text(&reply);
    assert!(
        !raw.starts_with(UNTRUSTED_FRAMING),
        "a json reply is a document to parse: {raw}"
    );

    let report: Js = serde_json::from_str(&raw).expect("json reply must parse");
    let mut strings: Vec<String> = Vec::new();
    collect_strings(&report, &mut strings);
    for value in &strings {
        assert!(
            !value
                .chars()
                .any(|c| c.is_ascii_control() && c != '\n' && c != '\t'),
            "a control character reached the reply: {value:?}"
        );
    }
    assert_eq!(
        report["topic"],
        json!("core  [31m forged  and\ttabbed"),
        "escapes, carriage return and DEL become spaces; the tab is left alone"
    );

    // And the same content on the digest path is sanitized as it always was.
    let text = task_reply(&one_task_call(db, "recall", json!({"topic": hostile})));
    assert!(
        !text.chars().any(|c| c.is_ascii_control() && c != '\n'),
        "the rendered digest keeps its own line structure and nothing else: {text:?}"
    );
}

/// Binding: the one control character a JSON value keeps is the newline, and
/// keeping it is what makes the report faithful.
///
/// `recall`'s report carries the whole rendered digest under `digest`, and
/// `context` carries quoted source. Those newlines are the document's own
/// structure, not something a contributor injected — a JSON value is delimited
/// by the grammar, so nothing inside one can forge a line the way it could in
/// a line-structured digest.
#[test]
fn a_json_reply_keeps_the_newlines_of_a_multi_line_value() {
    let db = code_store("json-multiline");
    let report = task_report(db, "recall", json!({"topic": "src/core.rs"}));
    let digest = report["digest"].as_str().expect("digest");
    assert!(
        digest.lines().count() > 2,
        "the rendered digest must survive as a document, not one flat line: {digest:?}"
    );
    assert!(digest.starts_with(UNTRUSTED_FRAMING), "{digest:?}");
}

/// Every string value in a JSON reply, at any depth.
fn collect_strings(value: &Js, out: &mut Vec<String>) {
    match value {
        Js::String(s) => out.push(s.clone()),
        Js::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Js::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

/// Binding: `json` is a boolean, and a caller that sends something else is told
/// so rather than served a digest it did not ask for.
#[test]
fn a_wrong_typed_json_argument_is_a_tool_error() {
    let reply = one_task_call(code_store("json-bad"), "map", json!({"json": "yes"}));
    assert!(error_text(&reply).contains("json must be a boolean"));
}

/// Binding: `recall` carries the framing line its own digest already emits, and
/// does not gain a second one.
#[test]
fn recall_is_framed_once_not_twice() {
    let db = code_store("recall-framing");
    let reply = one_task_call(db.clone(), "recall", json!({"topic": "src/core.rs"}));
    let full = task_text(&reply);
    assert_eq!(full.matches(UNTRUSTED_FRAMING).count(), 1, "{full}");
    // The digest core-api produced is what was shown, unaltered.
    assert_eq!(
        task_report(db, "recall", json!({"topic": "src/core.rs"}))["digest"],
        json!(full),
        "recall's own digest already opens with the framing line"
    );
}

/// Binding: every history reply says where history starts, and `was_linked`
/// refuses an unreachable commit by naming the range it accepts.
#[test]
fn history_replies_carry_the_horizon() {
    let db = open("mcp-horizon");
    seed_person(&db, "alice");
    seed_person(&db, "bob");
    db.write().insert_edge("LINK", "alice", "bob").unwrap();

    let stdin = format!(
        "{}{}{}",
        call(1, "node_history", json!({"key": "alice"})),
        call(2, "edge_history", json!({"a": "alice", "b": "bob"})),
        call(
            3,
            "was_linked",
            json!({"a": "alice", "b": "bob", "edge_type": "LINK", "at_commit": 99999})
        ),
    );
    let (res, out) = exchange(db, &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);

    let nh = content_json(&replies[0]);
    assert_eq!(nh["horizon"], json!(0), "node_history must report it: {nh}");
    assert!(nh["history"].is_array(), "{nh}");
    let eh = content_json(&replies[1]);
    assert_eq!(eh["horizon"], json!(0), "edge_history must report it: {eh}");

    let err = error_text(&replies[2]);
    assert!(
        err.contains("valid range is"),
        "the refusal must name the range it accepts: {err}"
    );
}

/// Binding: a role narrowed by `visible_where` answers through `query` with
/// only the nodes that pass the predicate, live and at a past commit alike.
#[test]
fn query_with_a_role_honours_a_visible_where_predicate() {
    let db = open("query-role-predicate");
    let at_one = {
        let mut w = db.write();
        w.insert_node(
            "Doc",
            "d1",
            vec![
                ("id".into(), Value::Str("d1".into())),
                ("status".into(), Value::Str("published".into())),
            ],
        )
        .unwrap();
        let at = w.wal_total_commits().unwrap() - 1;
        w.insert_node(
            "Doc",
            "d2",
            vec![
                ("id".into(), Value::Str("d2".into())),
                ("status".into(), Value::Str("draft".into())),
            ],
        )
        .unwrap();
        // No status at all: absent is not a match.
        w.insert_node("Doc", "d3", vec![("id".into(), Value::Str("d3".into()))])
            .unwrap();
        w.apply_schema(&core_api::Schema {
            roles: vec![core_api::RoleDef {
                name: "publisher".into(),
                keys: vec![],
                labels: vec!["Doc".into()],
                visible_where: Some(core_api::PropPredicate {
                    field: "status".into(),
                    eq: None,
                    in_: Some(vec![Value::Str("published".into())]),
                }),
                write: None,
            }],
            ..Default::default()
        })
        .unwrap();
        at
    };

    let rows = content_json(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n) RETURN n.id ORDER BY n.id", "role": "publisher"}),
    ));
    assert_eq!(
        rows["rows"],
        json!([["d1"]]),
        "only the published document passes the predicate"
    );

    // The predicate is part of the same one resolver, so as_of honours it too.
    let then = content_json(&one_task_call(
        db,
        "query",
        json!({"cypher": "MATCH (n) RETURN n.id", "role": "publisher", "as_of": at_one}),
    ));
    assert_eq!(then["rows"], json!([["d1"]]));
}
