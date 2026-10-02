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

use core_api::digest::UNTRUSTED_FRAMING;
use core_api::{Direction, SharedDb, Value, ViewDef, ViewSource};
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
/// `mushroomdb mcp <db>` passes.
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
        "pairwise_similar",
        "explain_association",
        "hybrid_search",
        "node_history",
        "edge_history",
        "was_linked",
        "rename_node",
    ] {
        assert!(names.contains(*expected), "missing tool: {expected}");
    }
    assert_eq!(tools.len(), 25);

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
            namespace: None,
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
            namespace: None,
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
// Task tools — the names in `TASK_TOOLS` below.
//
// These answer a question in prose rather than in JSON, so they come
// first in `--all-tools` and the graph tools listed beside them are prefixed
// `Advanced:`. Each returns the rendered digest as its text content and
// nothing else; a caller that wants the report passes `json: true` and gets
// it *as* the text.
// ─────────────────────────────────────────────────────────────────────────────

/// The task tools, in the order `tools/list` must list them.
const TASK_TOOLS: [&str; 11] = [
    "explain_association",
    "node_edges",
    "neighborhood",
    "edges_at",
    "what_if",
    "recall",
    "remember",
    "schema",
    "analyze",
    "suggest_rules",
    "forget",
];

/// The fourteen graph tools, in their established order, after the task tools.
const ADVANCED_TOOLS: [&str; 14] = [
    "query",
    "ingest_json",
    "create_rule",
    "explain",
    "stats",
    "node_info",
    "upsert_entity",
    "find_similar",
    "pairwise_similar",
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
            namespace: None,
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
    // No `about_*` rules: ingest-git stopped declaring them in 0.7, because
    // `remember` inserts its own ABOUT edges and a rule would own them.
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
        namespace: None,
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
        namespace: None,
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
            // Deliberately not a real path: nothing here reads the working
            // tree, so every tool must answer from the graph alone.
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

/// Binding: a memory store lists the association surface — the tools
/// that answer a question about an entity graph or fill one, in that order —
/// and none of the removed code-graph tools.
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
        server::ASSOCIATION_TOOLS.to_vec(),
        "default tools/list on a memory store"
    );
    assert_eq!(tools.len(), 23);
    for hidden in [
        "explore", "map", "context", "impact", "owners", "why", "sync",
    ] {
        assert!(
            !names.contains(&hidden),
            "{hidden} was removed in 0.7 and must not be listed"
        );
    }
}

/// A store of one of the two shapes a server can be started on: `"memory"`,
/// empty, or `"ingested"`, carrying the `GitSync` marker `ingest-git` writes.
/// The marker is all that ever told the two apart, so it is all this needs.
fn store_of_shape(shape: &str, name: &str) -> SharedDb {
    let db = open(&format!("{name}-{shape}"));
    match shape {
        "memory" => {}
        "ingested" => {
            db.write()
                .insert_node(
                    "GitSync",
                    "__mushroomdb_git_sync__",
                    vec![("id".into(), s("__mushroomdb_git_sync__"))],
                )
                .expect("marker");
        }
        other => panic!("no store shape {other}"),
    }
    db
}

/// The names a real `tools/list` advertises, default or `--all-tools`.
fn list_tool_names(db: &SharedDb, all_tools: bool) -> Vec<String> {
    let stdin = req(json!(1), "tools/list", None);
    let (res, out) = if all_tools {
        exchange_all_tools(db.clone(), &stdin)
    } else {
        exchange(db.clone(), &stdin)
    };
    assert!(res.is_ok(), "{res:?}");
    parse_lines(&out)[0]["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect()
}

/// One `tools/call` against a server started with or without `--all-tools`.
fn call_tool(db: &SharedDb, all_tools: bool, name: &str, arguments: &Js) -> Js {
    let stdin = call(1, name, arguments.clone());
    let (res, out) = if all_tools {
        exchange_all_tools(db.clone(), &stdin)
    } else {
        exchange(db.clone(), &stdin)
    };
    assert!(res.is_ok(), "{res:?}");
    parse_lines(&out).remove(0)
}

/// The seven code-graph tools are gone from both listings and from the served
/// set, on both shapes of store.
///
/// Asserted against a real `tools/list` rather than against a constant this
/// file declares. The 0.6.12 version of exactly this test compared against
/// its own copy of the tool list and passed with the bug reintroduced
/// (`docs/roadmap/road-to-0.7-plan.md:186-193`).
#[test]
fn the_code_graph_tools_are_absent_from_every_listing() {
    const GONE: [&str; 7] = [
        "explore", "map", "context", "impact", "owners", "why", "sync",
    ];

    for all_tools in [false, true] {
        for store_shape in ["memory", "ingested"] {
            let db = store_of_shape(store_shape, &format!("gone-{all_tools}"));
            let listed = list_tool_names(&db, all_tools);
            for name in GONE {
                assert!(
                    !listed.contains(&name.to_string()),
                    "{name} still listed (all_tools={all_tools}, store={store_shape}): {listed:?}"
                );
            }
            // And a call must be refused, not merely unlisted: "served but
            // hidden" is what these were before.
            let reply = call_tool(&db, all_tools, "explore", &json!({"target": "x"}));
            assert!(
                reply.get("error").is_some() || reply["result"]["isError"].as_bool() == Some(true),
                "explore is still answering (all_tools={all_tools}, store={store_shape}): {reply}"
            );
        }
    }
}

/// An `ingest-git` store now gets the same listing as any other. There is one
/// surface in 0.7.
#[test]
fn an_ingested_store_is_served_the_association_listing() {
    let db = store_of_shape("ingested", "one-surface");
    assert_eq!(
        list_tool_names(&db, false),
        server::ASSOCIATION_TOOLS.to_vec()
    );
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
            namespace: None,
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
                namespace: None,
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
            namespace: None,
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
            namespace: None,
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
            namespace: None,
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
            namespace: None,
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
    // same call succeeds without one.
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
            namespace: None,
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
    const OPENERS: [(&str, &str); 23] = [
        ("query", "Who may see this"),
        ("explain_association", "Why are A and B related"),
        ("neighborhood", "What is around K"),
        ("node_info", "What is K —"),
        ("node_edges", "What is K related to"),
        (
            // Leads with the date, as `edges_at` below does and for its
            // reason: `at_commit` takes one, and its schema now says so.
            "was_linked",
            "Were A and B linked on DATE (or at commit C)",
        ),
        (
            // Broadened in v0.6.11: `at` takes a date as well as a commit
            // index, and the description must lead with the date. Opening with
            // "at commit C" is the framing that made an agent reconstruct a
            // date->commit map by hand and guess wrong.
            "edges_at",
            "What did K's relationships look like on DATE (or at commit C)",
        ),
        ("what_if", "What changes if K's FIELD became VALUE"),
        ("node_history", "What has happened to K"),
        ("edge_history", "When did A and B become linked"),
        ("find_similar", "What is most like this"),
        (
            "pairwise_similar",
            "Which of these are most like each other",
        ),
        ("hybrid_search", "What matches these words and this vector"),
        ("remember", "Remember this for next time"),
        ("recall", "What do I already know about this"),
        ("upsert_entity", "Record what is now true about K"),
        ("ingest_json", "Fill the store from a batch"),
        (
            "create_rule",
            "How should this kind of relationship be derived from now on",
        ),
        ("stats", "How big is this store"),
        ("schema", "What's in here"),
        ("analyze", "What matters here, and what clusters"),
        ("suggest_rules", "What relationships are in my data"),
        ("forget", "Forget that"),
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
        server::ASSOCIATION_TOOLS.to_vec(),
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
                namespaces: None,
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
                namespaces: None,
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

/// Binding: an unlisted tool is still served. The flag decides what is
/// advertised, not what a caller that knows the name can reach. `explain` is
/// one of the two served tools the default listing leaves out.
#[test]
fn an_unlisted_graph_tool_is_still_callable() {
    let (res, out) = exchange(
        association_store("list-unlisted"),
        &call(1, "explain", json!({"a": "p1", "b": "acme"})),
    );
    assert!(res.is_ok(), "{res:?}");
    let reply = parse_lines(&out).remove(0);
    assert!(
        !reply["result"]["isError"].as_bool().unwrap_or(false),
        "explain is unlisted by default but must still answer: {reply}"
    );
}

/// Binding: `--all-tools` lists every served tool, task tools first in their fixed order, and
/// every one of the fourteen graph tools carries the `Advanced:` prefix.
#[test]
fn tools_list_has_every_tool_task_tools_first_and_advanced_prefix() {
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
    assert_eq!(tools.len(), 25);

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
        by_name("recall")["inputSchema"]["required"],
        json!(["topic"])
    );
    assert_eq!(
        by_name("remember")["inputSchema"]["required"],
        json!(["text"])
    );
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
/// called as, so it is compared against that list rather than the association
/// listing a default session sees.
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

/// Binding: the server card's `version` matches the crate version the tests were
/// compiled against.
#[test]
fn server_card_version_matches_crate_version() {
    let card_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.well-known/mcp/server-card.json");
    let card: Js = serde_json::from_str(&std::fs::read_to_string(&card_path).expect("server card"))
        .expect("server card json");
    let card_version = card["version"].as_str().expect("card version string");
    assert_eq!(
        card_version,
        env!("CARGO_PKG_VERSION"),
        "server-card.json version is {card_version} but crate is {}",
        env!("CARGO_PKG_VERSION"),
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
    // `text` here is the digest with its framing line stripped by `task_reply`.
    // `recall_digest` itself never carries that line — the framing is stamped
    // by the text wrapper, not by core-api's digest — so the json reply's own
    // `digest` field is the same body, unframed either way.
    assert_eq!(
        structured["digest"],
        json!(text),
        "the json reply's digest is the same body the framed text carries"
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
/// keys are created as provisional entities rather than refusing the call.
#[test]
fn remember_writes_note_and_stubs_unknown_about() {
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

    // Two unknown keys: both are named as provisional and written, not refused.
    let reply = one_task_call(
        db.clone(),
        "remember",
        json!({"text": "about nothing that exists yet", "about": ["zzz.rs", "no/such.rs"]}),
    );
    let text = task_reply(&reply);
    assert!(
        text.contains("zzz.rs") && text.contains("no/such.rs") && text.contains("provisional"),
        "{text}"
    );
    let g = db.read();
    assert_eq!(
        g.get_prop("zzz.rs", core_api::memory_schema::PROVISIONAL_PROP),
        Some(Value::Bool(true)),
        "an unknown about key must land as a provisional entity"
    );
    assert_eq!(
        g.get_prop("no/such.rs", core_api::memory_schema::PROVISIONAL_PROP),
        Some(Value::Bool(true)),
        "an unknown about key must land as a provisional entity"
    );
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
        "explain_association" => json!({"a": "src/core.rs", "b": "src/web.rs"}),
        "recall" => json!({"topic": "src/core.rs"}),
        "remember" => json!({"text": "framing check", "about": ["src/core.rs"]}),
        "node_edges" | "neighborhood" => json!({"key": "src/core.rs"}),
        "edges_at" => json!({"key": "src/core.rs", "at": 0}),
        "what_if" => json!({"key": "src/core.rs", "field": "lines", "value": 2}),
        "analyze" => json!({"kind": "central"}),
        // Last in the sweep, and a property the node does not carry: it
        // answers without removing anything a later tool would read.
        "forget" => json!({"key": "src/core.rs", "prop": "no_such_prop"}),
        _ => json!({}),
    };
    for tool in TASK_TOOLS {
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
    let (digest, report) = task_both(db, "node_edges", json!({"key": "src/core.rs"}));
    assert!(digest.contains("src/core.rs"), "{digest}");
    assert_eq!(report["key"], json!("src/core.rs"));
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
/// `recall`'s report carries the whole rendered digest under `digest`. Those
/// newlines are the document's own structure, not something a contributor
/// injected — a JSON value is delimited by the grammar, so nothing inside one
/// can forge a line the way it could in a line-structured digest.
#[test]
fn a_json_reply_keeps_the_newlines_of_a_multi_line_value() {
    let db = code_store("json-multiline");
    let report = task_report(db.clone(), "recall", json!({"topic": "src/core.rs"}));
    let digest = report["digest"].as_str().expect("digest");
    assert!(
        digest.lines().count() > 2,
        "the rendered digest must survive as a document, not one flat line: {digest:?}"
    );
    // The framing line is a transport concern the text wrapper owns now, not
    // core-api's digest — a json reply never carries it (see
    // `a_json_reply_is_unframed_and_sanitized`), so it is the plain-text reply
    // of the same call that must open with it.
    let text = task_text(&one_task_call(
        db,
        "recall",
        json!({"topic": "src/core.rs"}),
    ));
    assert!(text.starts_with(UNTRUSTED_FRAMING), "{text:?}");
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
    let reply = one_task_call(
        code_store("json-bad"),
        "recall",
        json!({"topic": "src/core.rs", "json": "yes"}),
    );
    assert!(error_text(&reply).contains("json must be a boolean"));
}

/// Binding: `recall`'s reply carries the framing line exactly once, first,
/// whether the topic matched or not. The digest itself never carries it — only
/// the text wrapper stamps it, and the wrapper no longer checks for a copy
/// already there — so this test is what keeps a second stamp from appearing.
#[test]
fn recall_is_framed_once_not_twice() {
    let db = code_store("recall-framing");
    let no_match = task_text(&one_task_call(
        db.clone(),
        "recall",
        json!({"topic": "zzqx-nothing-matches-this"}),
    ));
    assert!(no_match.starts_with(UNTRUSTED_FRAMING), "{no_match}");
    assert_eq!(no_match.matches(UNTRUSTED_FRAMING).count(), 1, "{no_match}");

    let reply = one_task_call(db.clone(), "recall", json!({"topic": "src/core.rs"}));
    let full = task_text(&reply);
    assert!(full.starts_with(UNTRUSTED_FRAMING), "{full}");
    assert_eq!(full.matches(UNTRUSTED_FRAMING).count(), 1, "{full}");
    // What is left after stripping the one framing line is exactly the json
    // reply's own digest — the same body, whichever way it was asked for.
    let body = task_reply(&reply);
    assert_eq!(
        task_report(db, "recall", json!({"topic": "src/core.rs"}))["digest"],
        json!(body),
        "the framed text and the json digest carry the same body"
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
                namespaces: None,
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

// ── Namespaces on the MCP surface ────────────────────────────────────────────

/// Three namespaces and two roles: `a-reader` bound to `tenant-a`, `everyone`
/// bound to nothing. Every namespace test in this file reads this one store, so
/// the only thing that differs between them is the argument under test.
///
/// `d-*` are in the implicit `default` namespace, `a-*` in `tenant-a`, `b-*` in
/// `tenant-b`.
fn ns_store(name: &str) -> SharedDb {
    let db = open(name);
    {
        let mut w = db.write();
        for (key, ns) in [
            ("d1", None),
            ("a1", Some("tenant-a")),
            ("a2", Some("tenant-a")),
            ("b1", Some("tenant-b")),
        ] {
            let mut props = vec![("id".into(), Value::Str(key.into()))];
            if let Some(ns) = ns {
                props.push(("ns".into(), Value::Str(ns.into())));
            }
            w.insert_node("Doc", key, props).unwrap();
        }
        w.apply_schema(&core_api::Schema {
            roles: vec![
                core_api::RoleDef {
                    name: "a-reader".into(),
                    keys: vec![],
                    labels: vec!["Doc".into()],
                    visible_where: None,
                    namespaces: Some(vec!["tenant-a".into()]),
                    write: None,
                },
                core_api::RoleDef {
                    name: "everyone".into(),
                    keys: vec![],
                    labels: vec!["Doc".into()],
                    visible_where: None,
                    namespaces: None,
                    write: None,
                },
            ],
            ..Default::default()
        })
        .unwrap();
    }
    db
}

fn ns_query(db: SharedDb, args: Js) -> Js {
    content_json(&one_task_call(db, "query", args))
}

/// Binding: `namespace` alone answers from that namespace only, and absent it
/// the same query still sees the whole store.
#[test]
fn query_with_a_namespace_answers_from_that_namespace_only() {
    let db = ns_store("query-namespace");
    let q = "MATCH (n) RETURN n.id ORDER BY n.id";

    let a = ns_query(db.clone(), json!({"cypher": q, "namespace": "tenant-a"}));
    assert_eq!(a["rows"], json!([["a1"], ["a2"]]));

    let b = ns_query(db.clone(), json!({"cypher": q, "namespace": "tenant-b"}));
    assert_eq!(b["rows"], json!([["b1"]]));

    let d = ns_query(db.clone(), json!({"cypher": q, "namespace": "default"}));
    assert_eq!(d["rows"], json!([["d1"]]), "absent `ns` means `default`");

    let all = ns_query(db.clone(), json!({"cypher": q}));
    assert_eq!(
        all["rows"],
        json!([["a1"], ["a2"], ["b1"], ["d1"]]),
        "no namespace argument is no namespace restriction"
    );

    // A name no node uses resolves to nothing, never to everything.
    let none = ns_query(db, json!({"cypher": q, "namespace": "tenant-z"}));
    assert_eq!(none["rows"], json!([]));
}

/// Binding: `namespace` intersects `role` — it can only narrow what the role
/// already allows, and a role bound to one namespace never sees another even
/// with no `namespace` argument of its own.
#[test]
fn query_with_a_role_and_a_namespace_intersects_and_never_unions() {
    let db = ns_store("query-role-namespace");
    let q = "MATCH (n) RETURN n.id ORDER BY n.id";

    let bound = ns_query(db.clone(), json!({"cypher": q, "role": "a-reader"}));
    assert_eq!(
        bound["rows"],
        json!([["a1"], ["a2"]]),
        "a namespaced role honours its binding with no argument here"
    );

    let inside = ns_query(
        db.clone(),
        json!({"cypher": q, "role": "a-reader", "namespace": "tenant-a"}),
    );
    assert_eq!(inside["rows"], json!([["a1"], ["a2"]]));

    let outside = ns_query(
        db.clone(),
        json!({"cypher": q, "role": "a-reader", "namespace": "tenant-b"}),
    );
    assert_eq!(
        outside["rows"],
        json!([]),
        "role ∩ namespace, never role ∪ namespace"
    );

    let unscoped = ns_query(
        db.clone(),
        json!({"cypher": q, "role": "everyone", "namespace": "tenant-b"}),
    );
    assert_eq!(
        unscoped["rows"],
        json!([["b1"]]),
        "an unscoped role is narrowed by the argument"
    );

    // A client mask is the other restriction, and the namespace narrows it too.
    let masked = ns_query(
        db.clone(),
        json!({"cypher": q, "mask": ["a1", "b1"], "namespace": "tenant-a"}),
    );
    assert_eq!(masked["rows"], json!([["a1"]]));

    let bad = error_text(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": q, "namespace": 42}),
    ));
    assert!(bad.contains("namespace"), "{bad}");

    let invalid = error_text(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": q, "namespace": "not a namespace!"}),
    ));
    assert!(invalid.contains("valid namespace name"), "{invalid}");

    // A namespace makes the call a read, as a mask does.
    let write = error_text(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "CREATE (x:Doc {id:'z1'})", "namespace": "tenant-a"}),
    ));
    assert!(write.contains("read-only"), "{write}");
    assert!(!db.read().has_node("z1"), "the write must not have landed");
}

/// Binding: `as_of` composes with `namespace`, alone and beside a role.
#[test]
fn query_as_of_composes_with_a_namespace() {
    let db = ns_store("query-asof-namespace");
    let at = {
        let mut w = db.write();
        let at = w.wal_total_commits().unwrap() - 1;
        w.insert_node(
            "Doc",
            "a3",
            vec![
                ("id".into(), Value::Str("a3".into())),
                ("ns".into(), Value::Str("tenant-a".into())),
            ],
        )
        .unwrap();
        at
    };
    let q = "MATCH (n) RETURN n.id ORDER BY n.id";

    let now = ns_query(db.clone(), json!({"cypher": q, "namespace": "tenant-a"}));
    assert_eq!(now["rows"], json!([["a1"], ["a2"], ["a3"]]));

    let then = ns_query(
        db.clone(),
        json!({"cypher": q, "namespace": "tenant-a", "as_of": at}),
    );
    assert_eq!(then["rows"], json!([["a1"], ["a2"]]), "as of before a3");

    let then_role = ns_query(
        db.clone(),
        json!({"cypher": q, "role": "a-reader", "namespace": "tenant-a", "as_of": at}),
    );
    assert_eq!(then_role["rows"], json!([["a1"], ["a2"]]));

    let then_foreign = ns_query(
        db,
        json!({"cypher": q, "role": "a-reader", "namespace": "tenant-b", "as_of": at}),
    );
    assert_eq!(
        then_foreign["rows"],
        json!([]),
        "the intersection is the intersection at that commit too"
    );
}

/// Binding: `stats` carries `namespaces` only when the call narrows — naming a
/// role or a namespace. An unscoped call omits the roster; the whole-store
/// counts beside it are unchanged either way.
#[test]
fn stats_carries_namespaces_and_narrows_under_a_role() {
    let db = ns_store("stats-namespaces");

    let all = content_json(&one_task_call(db.clone(), "stats", json!({})));
    assert!(
        all.get("namespaces").is_none(),
        "an unscoped call is told the counts, not who the tenants are: {all}"
    );

    let scoped = content_json(&one_task_call(
        db.clone(),
        "stats",
        json!({"role": "a-reader"}),
    ));
    assert_eq!(
        scoped["namespaces"],
        json!([{"name": "tenant-a", "nodes_live": 2}]),
        "a role bound to one namespace is told about that one"
    );
    assert_eq!(
        scoped["nodes_live"], all["nodes_live"],
        "narrowing the roster does not change the store-wide counts"
    );

    let one = content_json(&one_task_call(
        db.clone(),
        "stats",
        json!({"namespace": "tenant-b"}),
    ));
    assert_eq!(
        one["namespaces"],
        json!([{"name": "tenant-b", "nodes_live": 1}])
    );

    let both = content_json(&one_task_call(
        db.clone(),
        "stats",
        json!({"role": "a-reader", "namespace": "tenant-b"}),
    ));
    assert_eq!(both["namespaces"], json!([]), "the intersection is empty");

    let unknown = error_text(&one_task_call(db, "stats", json!({"role": "nobody"})));
    assert!(unknown.contains("unknown role 'nobody'"), "{unknown}");
}

/// Binding: a store that names no namespace still has exactly one, `default`,
/// and says so when a call asks for it. Unscoped, it says nothing about the
/// roster at all — a store with no namespaces and a store with ten answer an
/// unscoped `stats` identically.
#[test]
fn stats_on_a_store_without_namespaces_reports_one() {
    let db = open("stats-no-namespaces");
    seed_person(&db, "alice");

    let unscoped = content_json(&one_task_call(db.clone(), "stats", json!({})));
    assert!(
        unscoped.get("namespaces").is_none(),
        "unscoped stats omits the roster even when it holds one name: {unscoped}"
    );

    let scoped = content_json(&one_task_call(db, "stats", json!({"namespace": "default"})));
    assert_eq!(
        scoped["namespaces"],
        json!([{"name": "default", "nodes_live": 1}])
    );
}

/// Binding: a full-authority MCP `query` MERGE without `ns` still lands in
/// `default`; naming `ns` in the pattern creates there. Role-scoped MERGE is
/// an HTTP surface (MCP `query` with `role` is read-only).
#[test]
fn merge_create_names_a_namespace_or_lands_in_default() {
    let db = open("mcp-merge-ns");
    content_json(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MERGE (n:Doc {id: 'plain'})"}),
    ));
    assert_eq!(db.read().namespace_of("plain").as_deref(), Some("default"));

    content_json(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MERGE (n:Doc {id: 'named', ns: 'tenant-a'})"}),
    ));
    assert_eq!(
        db.read().namespace_of("named").as_deref(),
        Some("tenant-a"),
        "a MERGE that names ns creates in that namespace"
    );
}

/// Binding: `upsert_entity` puts a node it creates in `namespace`, and refuses
/// to move one that already exists — with the engine's own text.
#[test]
fn upsert_entity_creates_in_a_namespace_and_refuses_to_change_it() {
    let db = open("upsert-namespace");

    let created = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "a9", "label": "Doc", "namespace": "tenant-a", "props": {"title": "t"}}),
    ));
    assert_eq!(created["created"], json!(true));
    assert_eq!(
        db.read().namespace_of("a9").as_deref(),
        Some("tenant-a"),
        "the created node is in the namespace the call named"
    );

    // Updating props in place is fine; naming the same namespace again is a no-op.
    let same = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "a9", "namespace": "tenant-a", "props": {"title": "u"}}),
    ));
    assert_eq!(same["created"], json!(false));

    let moved = error_text(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "a9", "namespace": "tenant-b", "props": {"title": "v"}}),
    ));
    assert!(
        moved.contains("a namespace is set at insert and cannot be changed"),
        "{moved}"
    );
    assert_eq!(db.read().namespace_of("a9").as_deref(), Some("tenant-a"));

    let invalid = error_text(&one_task_call(
        db,
        "upsert_entity",
        json!({"key": "zz", "label": "Doc", "namespace": "no spaces", "props": {}}),
    ));
    assert!(invalid.contains("valid namespace name"), "{invalid}");
}

/// Binding: `ingest_json` puts every node it creates in `namespace`.
#[test]
fn ingest_json_puts_every_created_node_in_a_namespace() {
    let db = open("ingest-namespace");
    let rows = r#"[{"id": "t1"}, {"id": "t2"}]"#;
    let report = content_json(&one_task_call(
        db.clone(),
        "ingest_json",
        json!({"label": "Doc", "rows_json": rows, "namespace": "tenant-a"}),
    ));
    assert_eq!(report["inserted"], json!(2), "{report}");
    let g = db.read();
    assert_eq!(g.namespace_of("t1").as_deref(), Some("tenant-a"));
    assert_eq!(g.namespace_of("t2").as_deref(), Some("tenant-a"));
    assert_eq!(g.namespaces(), vec!["default", "tenant-a"]);
    drop(g);

    let invalid = error_text(&one_task_call(
        db,
        "ingest_json",
        json!({"label": "Doc", "rows_json": rows, "namespace": ""}),
    ));
    assert!(invalid.contains("valid namespace name"), "{invalid}");
}

/// Binding: `create_rule` takes `namespace`, and a scoped rule derives only
/// inside it.
#[test]
fn create_rule_accepts_a_namespace() {
    let db = open("create-rule-namespace");
    {
        let mut w = db.write();
        for (key, ns) in [("x1", "tenant-a"), ("x2", "tenant-a"), ("y1", "tenant-b")] {
            w.insert_node(
                "Doc",
                key,
                vec![
                    ("city".into(), Value::Str("berlin".into())),
                    ("ns".into(), Value::Str(ns.into())),
                ],
            )
            .unwrap();
        }
    }
    let ok = content_json(&one_task_call(
        db.clone(),
        "create_rule",
        json!({
            "name": "same_city_a",
            "src_label": "Doc",
            "dst_label": "Doc",
            "predicate": {"FieldEqual": {"field": "city"}},
            "edge_type": "SAME_CITY",
            "namespace": "tenant-a",
        }),
    ));
    assert_eq!(ok["ok"], json!(true));
    assert_eq!(
        db.read().rules()[0].namespace.as_deref(),
        Some("tenant-a"),
        "the rule stores its namespace"
    );
    let pairs = content_json(&one_task_call(
        db,
        "query",
        json!({"cypher": "MATCH (a)-[:SAME_CITY]->(b) RETURN a.id, b.id"}),
    ));
    let rows = pairs["rows"].as_array().expect("rows").clone();
    assert!(
        rows.iter()
            .all(|r| r[0] != json!("y1") && r[1] != json!("y1")),
        "a scoped rule never reaches the other namespace: {rows:?}"
    );
}

/// Binding: the `query` and the write schemas advertise `namespace`, so a
/// client knows to pass it. The tool *lists* are untouched — pinned elsewhere.
#[test]
fn the_schemas_advertise_namespace() {
    let db = open("schema-namespace");
    let stdin = req(json!(1), "tools/list", None);
    let (res, out) = exchange_all_tools(db, &stdin);
    assert!(res.is_ok(), "{res:?}");
    let reply = parse_lines(&out).remove(0);
    let tools = reply["result"]["tools"].as_array().expect("tools").clone();
    for name in [
        "query",
        "upsert_entity",
        "ingest_json",
        "create_rule",
        "stats",
    ] {
        let tool = tools
            .iter()
            .find(|t| t["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} is listed"));
        assert_eq!(
            tool["inputSchema"]["properties"]["namespace"]["type"],
            json!("string"),
            "{name} advertises a string `namespace`"
        );
    }

    // `stats` narrows its roster by role as well, so it advertises both — a
    // client that is shown only `namespace` cannot ask the question it is for.
    let stats = tools
        .iter()
        .find(|t| t["name"] == json!("stats"))
        .expect("stats is listed");
    assert_eq!(
        stats["inputSchema"]["properties"]["role"]["type"],
        json!("string"),
        "stats advertises a string `role`"
    );
    assert!(
        stats["description"]
            .as_str()
            .unwrap_or_default()
            .contains("namespaces"),
        "stats says the body carries namespaces: {}",
        stats["description"]
    );

    // And `upsert_entity` says that `id` is the key, not a property it writes.
    let upsert = tools
        .iter()
        .find(|t| t["name"] == json!("upsert_entity"))
        .expect("upsert_entity is listed");
    let desc = upsert["description"].as_str().unwrap_or_default();
    assert!(
        desc.contains("'id' in 'props' is ignored on both paths"),
        "{desc}"
    );
    assert!(
        desc.contains("atomically") || desc.contains("atomic"),
        "upsert_entity says the update is atomic: {desc}"
    );
}

/// Binding: `upsert_entity` never takes `id` from `props` — on the update path
/// either, which is where it used to. `id` is the node's key: the create path
/// stores it from `key`, `rename_node` is the only way to change it, and a
/// `props.id` naming anything else is dropped on both paths rather than leaving a
/// stored `id` that disagrees with the key it was reached by.
#[test]
fn upsert_entity_never_takes_id_from_props() {
    let db = open("upsert-id");

    // Create: `props.id` has always been dropped here.
    let created = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "alice", "label": "Person", "props": {"id": "not-alice", "team": "red"}}),
    ));
    assert_eq!(created["created"], json!(true));
    let info = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "alice", "props": {"id": "not-alice"}}),
    ));
    assert_eq!(
        info["updated_fields"],
        json!(0),
        "an update of nothing but `id` updates nothing: {info}"
    );
    assert_eq!(info["created"], json!(false));

    let node = content_json(&one_task_call(
        db.clone(),
        "node_info",
        json!({"key": "alice"}),
    ));
    assert_eq!(
        node["props"]["id"],
        json!("alice"),
        "the stored `id` is the key the node was created under, never the \
         `props.id` that disagreed with it: {node}"
    );
    assert_eq!(node["props"]["team"], json!("red"));

    // And `id` beside a real prop is dropped while the real one is written.
    let both = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "alice", "props": {"id": "not-alice", "team": "blue"}}),
    ));
    assert_eq!(both["updated_fields"], json!(1), "{both}");
    let node = content_json(&one_task_call(db, "node_info", json!({"key": "alice"})));
    assert_eq!(node["props"]["team"], json!("blue"));
    assert_eq!(node["props"]["id"], json!("alice"), "still the key: {node}");
}

/// Binding: an `upsert_entity` update is all-or-nothing. A refusal partway
/// (a different `ns`, a property the caller may not write) leaves the node
/// untouched: no sibling property lands, `commit_seq` does not move, and
/// `updated_fields` is not reported. The assertion is on the final state, not
/// on the order the props object happens to iterate.
#[test]
fn upsert_entity_update_is_all_or_nothing() {
    let db = open("upsert-atomic");
    let created = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "n1", "label": "Doc", "props": {"a": 1}}),
    ));
    assert_eq!(created["created"], json!(true));
    let before = db.read().commit_seq();

    // `b` sorts before `ns` in the BTreeMap the tool builds, so a
    // one-property-at-a-time loop commits `b` and then refuses `ns`. The
    // test must not depend on that order: whatever the map does, the node
    // after the call is the node before the call.
    let refused = one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "n1", "props": {"b": 2, "ns": "other"}}),
    );
    let msg = error_text(&refused);
    assert!(
        msg.contains("a namespace is set at insert and cannot be changed"),
        "{msg}"
    );
    assert!(
        refused["result"].get("updated_fields").is_none(),
        "a refusal is not an update: {refused}"
    );
    let node = content_json(&one_task_call(
        db.clone(),
        "node_info",
        json!({"key": "n1"}),
    ));
    assert!(
        node["props"].get("b").is_none(),
        "a refused ns change must not leave b committed: {node}"
    );
    assert_eq!(node["props"]["a"], json!(1));
    assert_eq!(
        db.read().commit_seq(),
        before,
        "a refused update must not take a commit"
    );

    // Second case: a property whose write is refused (a view-owned name —
    // the same per-property refusal a role token hits when it may not write
    // one field of a multi-prop update) refuses the whole call. `pop` sorts
    // after `b`, so a sequential loop would land `b` and then refuse `pop`.
    {
        let mut w = db.write();
        w.create_view(ViewDef {
            name: "doc_deg".into(),
            label: "Doc".into(),
            view_prop: "pop".into(),
            source: ViewSource::Degree {
                edge_type: "REL".into(),
                direction: Direction::Out,
            },
        })
        .unwrap();
    }
    let before_view = db.read().commit_seq();
    let view_refused = one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "n1", "props": {"b": 2, "pop": 9}}),
    );
    let view_msg = error_text(&view_refused);
    assert!(
        view_msg.contains("managed by view") && view_msg.contains("doc_deg"),
        "{view_msg}"
    );
    assert!(
        view_refused["result"].get("updated_fields").is_none(),
        "a refusal is not an update: {view_refused}"
    );
    let node = content_json(&one_task_call(
        db.clone(),
        "node_info",
        json!({"key": "n1"}),
    ));
    assert!(
        node["props"].get("b").is_none(),
        "a refused reserved-name write must not leave b committed: {node}"
    );
    assert_eq!(node["props"]["a"], json!(1));
    assert_eq!(
        db.read().commit_seq(),
        before_view,
        "a refused update must not take a commit"
    );
}

/// Binding: naming the namespace a node is already in writes nothing and counts
/// nothing — the engine's no-op, reported as one.
#[test]
fn upsert_entity_does_not_count_a_no_op_namespace() {
    let db = open("upsert-ns-noop");
    let _ = one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "a1", "label": "Doc", "namespace": "tenant-a", "props": {"title": "t"}}),
    );
    let before = db.read().commit_seq();

    let same = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "a1", "namespace": "tenant-a", "props": {}}),
    ));
    assert_eq!(
        same["updated_fields"],
        json!(0),
        "the namespace it is already in is not an update: {same}"
    );
    assert_eq!(
        db.read().commit_seq(),
        before,
        "and it takes no commit either"
    );

    // Beside a real prop, only the real one counts.
    let one = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "a1", "namespace": "tenant-a", "props": {"title": "u"}}),
    ));
    assert_eq!(one["updated_fields"], json!(1), "{one}");
    assert_eq!(db.read().namespace_of("a1").as_deref(), Some("tenant-a"));
}

/// Binding: a provisional entity `remember` creates is stuck at its
/// creation-time label forever — the engine has no label-mutation path at
/// all — so `upsert_entity` naming a different label on an existing key is a
/// refusal, not a silent accept-and-drop. Fix round 3, 0.7 (corrected from
/// fix round 2's weaker "disclose it" call): a supplied `label` was
/// previously ignored on *every* update, not only a provisional stub's, and
/// `{"ok":true,"created":false}` beside a dropped field is not a signal an
/// agent acts on for a mistake this permanent. On a clean binary,
/// `remember{about:["matthew"]}` then
/// `upsert_entity{key:"matthew", label:"Person", ...}` used to report
/// `{"ok":true,"created":false}` and leave the node permanently `Entity`,
/// invisible to `MATCH (n:Person)`.
#[test]
fn upsert_entity_on_an_existing_key_with_a_different_label_is_refused() {
    let db = open("upsert-label-mismatch");

    // An unknown `about` key is created provisional, label `Entity`.
    let remembered = one_task_call(
        db.clone(),
        "remember",
        json!({"text": "Matthew reviewed it", "about": ["matthew"]}),
    );
    task_reply(&remembered); // panics if `remember` errored
    assert_eq!(
        db.read().node_info("matthew").map(|n| n.label),
        Some("Entity".to_string())
    );

    let refused = one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew", "label": "Person", "props": {"name": "Matthew Sherlin"}}),
    );
    let msg = error_text(&refused);
    assert!(
        msg.contains("Person") && msg.contains("Entity"),
        "the refusal must name both the requested and the stored label: {msg}"
    );

    // All-or-nothing, like every other pre-write refusal this tool makes: the
    // label could not be honoured, so none of `props` was written either.
    let g = db.read();
    assert_eq!(
        g.node_info("matthew").map(|n| n.label),
        Some("Entity".to_string())
    );
    assert_eq!(
        g.get_prop("matthew", "name"),
        Some(Value::Str("matthew".into())),
        "a refused label change must not leave props partially written — \
         `name` must still be the provisional stub's own key, not \"Matthew Sherlin\""
    );
    drop(g);
    let count = content_json(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n:Entity) RETURN count(n) AS c"}),
    ));
    assert_eq!(
        count["rows"],
        json!([[1]]),
        "the node must still be reachable by its real label: {count}"
    );

    // Naming the label the node already has still works — only a *different*
    // label is refused.
    let same = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew", "label": "Entity", "props": {"name": "Matthew Sherlin"}}),
    ));
    assert_eq!(same["ok"], json!(true), "{same}");
    assert_eq!(same["label"], json!("Entity"), "{same}");
}

/// Binding: the correct path — naming the label in `remember`'s `entities` at
/// first mention — produces a correctly labelled, non-provisional node with
/// its `ABOUT` edge intact, and `recall` finds it. This is the escape the
/// refusal above points callers to.
#[test]
fn remember_entities_names_the_label_correctly_the_first_time() {
    let db = open("remember-entities-label");
    let remembered = one_task_call(
        db.clone(),
        "remember",
        json!({
            "text": "Matthew reviewed the launch copy",
            "about": ["matthew"],
            "entities": [{"key": "matthew", "label": "Person", "props": {"name": "Matthew Sherlin"}}]
        }),
    );
    task_reply(&remembered);

    let g = db.read();
    assert_eq!(
        g.node_info("matthew").map(|n| n.label),
        Some("Person".to_string()),
        "an entity named at first mention must be created under the label given, not `Entity`"
    );
    assert_eq!(
        g.get_prop("matthew", core_api::memory_schema::PROVISIONAL_PROP),
        None,
        "a described entity must never be marked provisional"
    );
    let edges = g.node_edges("matthew").expect("node_edges");
    assert!(
        edges.iter().any(|e| e.edge_type == "ABOUT"),
        "the ABOUT edge must still land: {edges:?}"
    );
    drop(g);

    let recalled = task_reply(&one_task_call(
        db,
        "recall",
        json!({"topic": "Matthew Sherlin"}),
    ));
    assert!(
        recalled.contains("matthew"),
        "recall must find the correctly-labelled entity: {recalled}"
    );
}

/// Binding: `stats` with a `role` on a store whose `roles.json` was corrupt at
/// open says what `query` with a `role` says — the cause, not "unknown role".
#[test]
fn stats_with_a_role_on_a_corrupt_roles_file_says_so() {
    let dir = tmp("stats-corrupt-roles");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("roles.json"), b"not valid json at all!").unwrap();
    let db = SharedDb::open(&dir).unwrap();
    seed_person(&db, "alice");

    let from_query = error_text(&one_task_call(
        db.clone(),
        "query",
        json!({"cypher": "MATCH (n) RETURN n", "role": "anyone"}),
    ));
    let from_stats = error_text(&one_task_call(db, "stats", json!({"role": "anyone"})));
    assert!(
        from_query.contains("roles.json"),
        "query names the cause: {from_query}"
    );
    assert_eq!(
        from_stats, from_query,
        "and `stats` says exactly the same thing"
    );
}

/// Binding: every tool the shipped skill tells an agent to call is a tool the
/// default listing advertises.
///
/// This is the test that was missing. Through v0.6.11 `SKILL.md`'s very first
/// instruction — what to do with an empty store, which is every new install —
/// named `ingest_json` and `upsert_entity`, and the memory surface advertised
/// neither. The server served them, so every handshake assertion passed; but a
/// host builds its tool set from `tools/list`, so the model could not call
/// them. The first thing a new user was told to do was the one thing that
/// could not be done.
///
/// The skill is compiled into the CLI with `include_str!`, so it is the same
/// bytes that ship. Names are read out of backticks, and only names the server
/// actually serves are considered — prose mentions of `mcp`, `role` or a
/// property name are not tool calls.
#[test]
fn every_tool_the_skill_names_is_advertised() {
    let skill_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../cli/skills/mushroom/SKILL.md");
    let skill = std::fs::read_to_string(&skill_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", skill_path.display()));

    // Everything the server can serve, so prose in backticks that happens not
    // to be a tool name is ignored.
    let names_from = |out: &[u8]| -> Vec<String> {
        parse_lines(out)[0]["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|t| t["name"].as_str().expect("name").to_string())
            .collect()
    };

    // Everything the server can serve, so prose in backticks that happens not
    // to be a tool name is ignored.
    let (res, out) = exchange_all_tools(open("skill-served"), &req(json!(1), "tools/list", None));
    assert!(res.is_ok(), "{res:?}");
    let served = names_from(&out);

    // What a memory store actually advertises — read from a real `tools/list`,
    // not from this file's copy of the list. Comparing the skill against a
    // constant the test itself declares would pass even if the server
    // advertised nothing.
    let (res, out) = exchange(open("skill-listed"), &req(json!(1), "tools/list", None));
    assert!(res.is_ok(), "{res:?}");
    let advertised = names_from(&out);

    let mut named: Vec<String> = Vec::new();
    for chunk in skill.split('`').skip(1).step_by(2) {
        let name = chunk.trim();
        if served.iter().any(|s| s == name) && !named.iter().any(|n| n == name) {
            named.push(name.to_string());
        }
    }
    assert!(
        !named.is_empty(),
        "the skill names no tools at all — this test has stopped testing anything"
    );

    let missing: Vec<&String> = named.iter().filter(|n| !advertised.contains(n)).collect();
    assert!(
        missing.is_empty(),
        "the skill tells an agent to call {missing:?}, which a memory store's \
         tools/list does not advertise. A host builds its tool set from that \
         listing, so the model cannot reach them however well the server \
         serves them. Either advertise them or stop naming them."
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// schema — "what's in here?"
// ─────────────────────────────────────────────────────────────────────────────

/// A store created the way `mushroomdb mcp` creates one: the memory schema
/// applied, nothing written.
fn memory_store(name: &str) -> SharedDb {
    let db = open(name);
    db.write()
        .apply_schema(&core_api::memory_schema::memory_defaults())
        .unwrap();
    db
}

/// A rule over `name`, written straight to the store as a test fixture.
fn same_name_rule() -> core_api::RuleDef {
    core_api::RuleDef {
        name: "same_name".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: core_api::Predicate::FieldEqual {
            field: "name".into(),
        },
        edge_type: "SAME_NAME".into(),
        weight_prop: Some("weight".into()),
        max_edges: Some(32),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

/// Binding: `schema` answers with what `stats` never carried — labels with
/// their fields, edge types, every rule with its predicate, the full-text and
/// equality declarations, and the provisional nodes `remember` stubbed.
#[test]
fn schema_names_labels_rules_indexes_and_provisional_nodes() {
    let db = memory_store("schema-full");
    db.write().create_rule(same_name_rule()).unwrap();
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "Reid reviewed the copy", "about": ["reid"]}),
    );
    seed_person(&db, "matthew");

    let text = task_reply(&one_task_call(db.clone(), "schema", json!({})));
    for want in [
        "labels:",
        "Person (1)",
        "Entity (1)",
        "rules:",
        "same_name: Person → Person derives SAME_NAME — field_equal on name (global)",
        "full-text (recall searches these):",
        "Note.text",
        "equality indexes:",
        "Person.name",
        "provisional: 1 — named but not yet described: reid",
    ] {
        assert!(text.contains(want), "schema missing {want:?}:\n{text}");
    }
}

/// Binding: the report behind it, for a program — counts, not prose.
#[test]
fn schema_json_carries_every_declaration() {
    let db = memory_store("schema-json");
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "two unknowns", "about": ["reid", "ada"]}),
    );
    let report = task_report(db, "schema", json!({}));
    assert_eq!(report["provisional"], json!(2));
    assert_eq!(report["provisional_sample"], json!(["ada", "reid"]));
    assert!(
        report["indexes"]
            .as_array()
            .unwrap()
            .contains(&json!(["Person", "name"])),
        "{report}"
    );
    assert!(
        report["fulltext"]
            .as_array()
            .unwrap()
            .contains(&json!(["Note", "text"])),
        "{report}"
    );
}

/// Binding: a store with nothing in it still answers, and says what it has
/// declared — a memory store is created with its indexes before any node.
#[test]
fn schema_on_an_empty_memory_store_lists_its_declarations() {
    let text = task_reply(&one_task_call(
        memory_store("schema-empty"),
        "schema",
        json!({}),
    ));
    assert!(text.contains("0 node(s)"), "{text}");
    assert!(text.contains("Person.name"), "{text}");
    assert!(
        !text.contains("provisional:"),
        "nothing provisional: {text}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// forget — "forget that"
// ─────────────────────────────────────────────────────────────────────────────

/// Review focus: forgetting a key many notes were about deletes the node and
/// names the notes — capped, counted — without touching their text.
#[test]
fn forgetting_a_key_many_notes_name_lists_them_and_keeps_them() {
    let db = memory_store("forget-many");
    for i in 0..25 {
        one_task_call(
            db.clone(),
            "remember",
            json!({"text": format!("Reid fact number {i}"), "about": ["reid"]}),
        );
    }
    let text = task_reply(&one_task_call(db.clone(), "forget", json!({"key": "reid"})));
    assert!(text.starts_with("forgot reid (Entity)"), "{text}");
    assert!(text.contains("25 note(s) still say it"), "{text}");
    assert!(
        text.contains("(+15 more)"),
        "ten named, fifteen counted: {text}"
    );
    assert!(text.contains("history still holds it"), "{text}");
    assert!(text.contains("`mushroomdb migrate`"), "{text}");
    let g = db.read();
    assert!(!g.has_node("reid"), "the node is gone");
    assert_eq!(
        g.nodes_with_label("Note").len(),
        25,
        "every note is still there"
    );
}

/// Binding: removing one property — the one deletion MCP had no path to,
/// because Cypher here has no REMOVE.
#[test]
fn forgetting_a_property_removes_only_that_property() {
    let db = memory_store("forget-prop");
    one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew", "label": "Person",
               "props": {"name": "Matthew", "email": "m@example.com"}}),
    );
    let text = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"key": "matthew", "prop": "email"}),
    ));
    assert!(text.starts_with("forgot matthew.email"), "{text}");
    assert_eq!(db.read().get_prop("matthew", "email"), None);
    assert_eq!(
        db.read().get_prop("matthew", "name"),
        Some(Value::Str("Matthew".into()))
    );

    let again = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"key": "matthew", "prop": "email"}),
    ));
    assert!(again.contains("is not set; nothing to forget"), "{again}");
    assert!(
        !again.contains("history still holds it"),
        "nothing was written, so there is nothing in history to warn about: {again}"
    );
}

/// A forgotten name takes its words out of the store-maintained `aliases` in
/// the same write, and the reply says so. Forgetting any other property says
/// nothing about aliases, and a declared alias is not touched.
#[test]
fn forgetting_a_name_takes_its_words_out_of_aliases_and_says_so() {
    let db = memory_store("forget-name");
    one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "jane", "label": "Person",
               "props": {"name": "Jane Q Public", "email": "j@example.com"},
               "aliases": ["JQP"]}),
    );
    assert_eq!(
        aliases_in(&db, "jane"),
        vec!["jane", "jane q public", "public", "q"]
    );
    let email = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"key": "jane", "prop": "email"}),
    ));
    assert!(!email.contains("aliases"), "{email}");

    let name = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"key": "jane", "prop": "name"}),
    ));
    assert!(name.starts_with("forgot jane.name"), "{name}");
    assert!(
        name.contains(
            "its words left `aliases` in the same write; `aliases` now holds what the key \
             alone implies\n"
        ),
        "{name}"
    );
    assert!(!name.contains("remain"), "{name}");
    assert_eq!(aliases_in(&db, "jane"), vec!["jane"], "the reply is true");
    assert_eq!(
        db.read().get_prop("jane", "alias_keys"),
        Some(Value::List(vec![Value::Str("JQP".into())])),
        "what was declared stays, and nothing of the name was added to it"
    );
    assert_eq!(db.read().get_prop("jane", "name"), None);

    // The json says the same, and a second forget has nothing to rewrite.
    one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "jane", "props": {"name": "Jane Q Public"}}),
    );
    let forget_name = json!({"key": "jane", "prop": "name"});
    let report = task_report(db.clone(), "forget", forget_name.clone());
    assert_eq!(report["aliases_rewritten"], json!(true), "{report}");
    let report = task_report(db.clone(), "forget", forget_name);
    assert_eq!(report["changed"], json!(false), "{report}");
    assert_eq!(report["aliases_rewritten"], json!(false), "{report}");

    // A node the memory tools never described has no `aliases`, and a forget
    // does not give it one.
    db.write()
        .insert_node(
            "Doc",
            "readme",
            vec![("name".into(), Value::Str("Read Me".into()))],
        )
        .unwrap();
    let doc = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"key": "readme", "prop": "name"}),
    ));
    assert!(doc.starts_with("forgot readme.name"), "{doc}");
    assert!(!doc.contains("aliases"), "{doc}");
    assert_eq!(db.read().get_prop("readme", "aliases"), None);
}

/// With the identity preset, forgetting a name retracts the link the name
/// made, in that same write, and the reply counts it.
#[test]
fn forgetting_a_name_retracts_the_link_it_made() {
    let db = linked_pair_store("forget-name-link");
    let text = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"key": "msherlin", "prop": "name"}),
    ));
    assert!(
        text.contains("2 derived edge(s) retracted because a rule read name or aliases"),
        "{text}"
    );
    assert_eq!(db.read().weighted_edges("SAME_AS", None).len(), 0);
}

/// Binding: retracting a fact removes the edge and names the notes whose text
/// still states it.
#[test]
fn forgetting_a_fact_retracts_the_edge_and_names_the_notes_behind_it() {
    let db = memory_store("forget-fact");
    one_task_call(
        db.clone(),
        "remember",
        json!({
            "text": "Matthew wants 0.7 to focus on the write path",
            "about": ["matthew", "v0.7"],
            "entities": [{"key": "matthew", "label": "Person"},
                         {"key": "v0.7", "label": "Release"}],
            "facts": [{"subject": "matthew", "predicate": "WANTS", "object": "v0.7"}]
        }),
    );
    let text = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"fact": {"subject": "matthew", "predicate": "WANTS", "object": "v0.7"}}),
    ));
    assert!(text.starts_with("retracted WANTS matthew → v0.7"), "{text}");
    assert!(text.contains("1 note(s) still say it"), "{text}");
    assert!(
        db.read()
            .neighbors("matthew", "WANTS", core_api::Direction::Out)
            .unwrap()
            .is_empty(),
        "the edge is gone"
    );
}

/// Binding: an edge a rule derived cannot be retracted by hand; the refusal
/// names the rule and the only two ways it can change.
#[test]
fn forgetting_a_rule_owned_fact_is_refused_with_the_rule_named() {
    let db = memory_store("forget-owned");
    db.write().create_rule(same_name_rule()).unwrap();
    for key in ["a", "b"] {
        db.write()
            .insert_node("Person", key, vec![("name".into(), Value::Str("X".into()))])
            .unwrap();
    }
    let err = error_text(&one_task_call(
        db.clone(),
        "forget",
        json!({"fact": {"subject": "a", "predicate": "SAME_NAME", "object": "b"}}),
    ));
    assert!(err.contains("derived by rule same_name"), "{err}");
    assert!(err.contains("(name)"), "the field the rule reads: {err}");
    assert!(err.contains("Nothing was written"), "{err}");
    assert_eq!(
        db.read()
            .neighbors("a", "SAME_NAME", core_api::Direction::Out)
            .unwrap(),
        vec!["b".to_string()]
    );
}

/// Binding: exactly one of the three shapes, and an unknown key is named.
#[test]
fn forget_takes_exactly_one_shape_and_names_an_unknown_key() {
    let db = memory_store("forget-shape");
    seed_person(&db, "a");
    for args in [
        json!({}),
        json!({"prop": "name"}),
        json!({"key": "a", "fact": {"subject": "a", "predicate": "X", "object": "a"}}),
    ] {
        let err = error_text(&one_task_call(db.clone(), "forget", args.clone()));
        assert!(err.contains("pass exactly one of"), "{args}: {err}");
    }
    let err = error_text(&one_task_call(db, "forget", json!({"key": "nobody"})));
    assert!(err.contains("nobody"), "{err}");
}

/// Binding: removing a property a rule reads retracts the edges that rule
/// derived from it, and the reply says so rather than only "forgot k.name".
#[test]
fn forgetting_a_property_a_rule_reads_reports_the_derived_edges_it_retracts() {
    let db = memory_store("forget-prop-derived");
    db.write().create_rule(same_name_rule()).unwrap();
    for key in ["a", "b"] {
        db.write()
            .insert_node("Person", key, vec![("name".into(), Value::Str("X".into()))])
            .unwrap();
    }
    assert_eq!(
        db.read()
            .neighbors("a", "SAME_NAME", core_api::Direction::Out)
            .unwrap(),
        vec!["b".to_string()],
        "precondition: the rule linked them"
    );
    let text = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"key": "a", "prop": "name"}),
    ));
    assert!(text.starts_with("forgot a.name"), "{text}");
    assert!(
        text.contains("derived edge(s) retracted because a rule read name"),
        "{text}"
    );
    assert!(!text.contains("0 derived edge(s) retracted"), "{text}");
    let g = db.read();
    assert!(g
        .neighbors("a", "SAME_NAME", core_api::Direction::Out)
        .unwrap()
        .is_empty());
    assert!(g
        .neighbors("b", "SAME_NAME", core_api::Direction::Out)
        .unwrap()
        .is_empty());
}

/// Binding: a property no rule reads retracts nothing, and the reply does not
/// claim it did.
#[test]
fn forgetting_a_property_no_rule_reads_reports_no_derived_edges() {
    let db = memory_store("forget-prop-plain");
    db.write().create_rule(same_name_rule()).unwrap();
    for key in ["a", "b"] {
        db.write()
            .insert_node(
                "Person",
                key,
                vec![
                    ("name".into(), Value::Str("X".into())),
                    ("email".into(), Value::Str(format!("{key}@example.com"))),
                ],
            )
            .unwrap();
    }
    let text = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"key": "a", "prop": "email"}),
    ));
    assert!(!text.contains("derived edge(s) retracted"), "{text}");
    assert_eq!(
        db.read()
            .neighbors("a", "SAME_NAME", core_api::Direction::Out)
            .unwrap(),
        vec!["b".to_string()]
    );
}

/// Binding: a fact naming a key the store does not have is refused with the
/// key named, and nothing is written.
#[test]
fn forgetting_a_fact_with_an_unknown_subject_names_it_and_writes_nothing() {
    let db = memory_store("forget-fact-unknown");
    seed_person(&db, "a");
    let (nodes, edges, seq) = {
        let g = db.read();
        (g.stats().nodes_live, g.stats().edges, g.commit_seq())
    };
    let err = error_text(&one_task_call(
        db.clone(),
        "forget",
        json!({"fact": {"subject": "ghost", "predicate": "KNOWS", "object": "a"}}),
    ));
    assert!(err.contains("ghost"), "{err}");
    let g = db.read();
    assert_eq!(g.stats().nodes_live, nodes);
    assert_eq!(g.stats().edges, edges);
    assert_eq!(g.commit_seq(), seq, "nothing was committed");
}

/// Binding: a fact whose ends exist but are not linked retracts nothing, says
/// so, and does not warn about history it did not write.
#[test]
fn forgetting_an_absent_fact_says_nothing_to_retract() {
    let db = memory_store("forget-fact-absent");
    seed_person(&db, "a");
    seed_person(&db, "b");
    let text = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"fact": {"subject": "a", "predicate": "KNOWS", "object": "b"}}),
    ));
    assert!(
        text.contains("no edge KNOWS a → b; nothing to retract"),
        "{text}"
    );
    assert!(!text.contains("history still holds it"), "{text}");
}

/// Binding: an absent fact is nothing to retract even when a rule's predicate
/// matches the pair. The engine's delete guard refuses before it looks for
/// the edge (ledger row 67); the reply is the absent-fact reply, byte for
/// byte, not a refusal about an edge "written by hand".
#[test]
fn forgetting_an_absent_fact_a_rule_matches_says_nothing_to_retract() {
    let reply = |name: &str, with_rule: bool| {
        let db = memory_store(name);
        for key in ["a", "b"] {
            db.write()
                .insert_node("Person", key, vec![("name".into(), Value::Str("X".into()))])
                .unwrap();
        }
        if with_rule {
            // A via-hop rule with no via node: it derives nothing.
            let mut rule = same_name_rule();
            rule.via_label = Some("Team".into());
            rule.via_edge = Some("MEMBER_OF".into());
            db.write().create_rule(rule).unwrap();
        }
        let seq = db.read().commit_seq();
        let text = task_reply(&one_task_call(
            db.clone(),
            "forget",
            json!({"fact": {"subject": "a", "predicate": "SAME_NAME", "object": "b"}}),
        ));
        assert_eq!(db.read().commit_seq(), seq, "nothing was committed");
        text
    };
    let text = reply("forget-absent-rule", true);
    assert!(
        text.contains("no edge SAME_NAME a → b; nothing to retract"),
        "{text}"
    );
    assert_eq!(text, reply("forget-absent-no-rule", false));
}

// ─────────────────────────────────────────────────────────────────────────────
// suggest_rules — "what relationships are in my data?"
// ─────────────────────────────────────────────────────────────────────────────

/// A memory store the way `remember` fills one — notes with kinds, sources
/// and timestamps — plus people with a field worth a rule.
fn suggest_store(name: &str) -> SharedDb {
    let db = memory_store(name);
    for i in 0..30 {
        let kind = ["note", "decision", "todo"][i % 3];
        let source = ["session-a", "session-b"][i % 2];
        one_task_call(
            db.clone(),
            "remember",
            json!({
                "text": format!("note {i} about the release"),
                "kind": kind,
                "source": source,
                "ts": 1_759_000_000 + i as i64,
            }),
        );
    }
    for i in 0..12 {
        let team = ["infra", "ui"][i % 2];
        one_task_call(
            db.clone(),
            "upsert_entity",
            json!({"key": format!("p{i}"), "label": "Person",
                   "props": {"name": format!("Person {i}"), "team": team}}),
        );
    }
    db
}

/// Binding (R5): on a store `remember` filled, nothing is proposed over a
/// field the store writes for itself — and the filter is shown to have fired.
#[test]
fn suggest_rules_never_proposes_a_bookkeeping_field() {
    let report = task_report(suggest_store("suggest-noise"), "suggest_rules", json!({}));
    let bookkeeping = [
        "ns",
        "kind",
        "ts",
        "source",
        "provisional",
        "id",
        "aliases",
        "alias_keys",
    ];
    for s in report["suggestions"].as_array().expect("suggestions") {
        let args = s["create_rule_args"].to_string();
        for f in bookkeeping {
            assert!(
                !args.contains(&format!("\"field\":\"{f}\"")),
                "proposed a rule over bookkeeping field {f}: {s}"
            );
        }
    }
    assert!(
        report["bookkeeping_hidden"].as_u64().unwrap() > 0,
        "this store's notes share kind, source and ts; the filter must have dropped those: {report}"
    );
    assert!(
        report["suggestions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["edge_type"] == "SAME_TEAM"),
        "the one real pattern survives: {report}"
    );
}

/// Binding (R5): a proposal's arguments are what `create_rule` takes — no
/// nulls, `weight_prop` written out — and passing them unchanged creates
/// exactly the rule proposed. The tool itself creates nothing.
#[test]
fn a_proposal_is_accepted_by_create_rule_unchanged() {
    let db = suggest_store("suggest-roundtrip");
    let rules_before = db.read().rules().len();
    let report = task_report(db.clone(), "suggest_rules", json!({}));
    assert_eq!(
        db.read().rules().len(),
        rules_before,
        "suggest_rules created a rule"
    );

    let s = report["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["edge_type"] == "SAME_TEAM")
        .expect("SAME_TEAM proposed")
        .clone();
    let args = s["create_rule_args"].clone();
    let obj = args.as_object().unwrap();
    assert!(obj.values().all(|v| !v.is_null()), "a null in {args}");
    assert!(obj.contains_key("weight_prop"), "{args}");
    assert!(!obj.contains_key("namespace"), "{args}");

    let (res, out) = exchange(db.clone(), &call(1, "create_rule", args.clone()));
    assert!(res.is_ok(), "{res:?}");
    let reply = parse_lines(&out).remove(0);
    assert_eq!(content_json(&reply)["ok"], json!(true), "{reply}");
    let created = db
        .read()
        .rules()
        .into_iter()
        .find(|r| r.name == s["name"].as_str().unwrap())
        .expect("the proposed rule now exists");
    assert_eq!(
        serde_json::to_value(&created.predicate).unwrap(),
        args["predicate"]
    );
}

/// Binding: every proposal says it is global, and the listing is bounded.
#[test]
fn proposals_say_they_are_global_and_stop_at_five() {
    let db = memory_store("suggest-many");
    for (i, label) in ["Red", "Green", "Blue"].iter().cycle().take(18).enumerate() {
        db.write()
            .insert_node(
                label,
                &format!("n{i}"),
                vec![("colour".into(), Value::Str(["warm", "cool"][i % 2].into()))],
            )
            .unwrap();
    }
    let (text, report) = task_both(db, "suggest_rules", json!({}));
    let listed = report["suggestions"].as_array().unwrap().len();
    assert_eq!(listed, 5, "{report}");
    assert!(report["total"].as_u64().unwrap() > 5, "{report}");
    assert_eq!(
        text.matches("global: links across namespaces").count(),
        5,
        "{text}"
    );
    assert!(text.contains("… and "), "the rest are counted: {text}");
}

// ─────────────────────────────────────────────────────────────────────────────
// analyze — "what matters here, what clusters?"
// ─────────────────────────────────────────────────────────────────────────────

/// Two linked groups — a star of five around `hub` and a pair — and four
/// nodes linked to nothing.
fn analyze_store(name: &str) -> SharedDb {
    let db = memory_store(name);
    {
        let mut g = db.write();
        for key in [
            "hub", "s1", "s2", "s3", "s4", "p1", "p2", "lone1", "lone2", "lone3", "lone4",
        ] {
            g.insert_node("Person", key, vec![("name".into(), Value::Str(key.into()))])
                .unwrap();
        }
        for s in ["s1", "s2", "s3", "s4"] {
            g.insert_edge("KNOWS", s, "hub").unwrap();
        }
        g.insert_edge("KNOWS", "p1", "p2").unwrap();
    }
    db
}

#[test]
fn analyze_central_ranks_the_hub_first_with_its_label() {
    let (text, report) = task_both(
        analyze_store("analyze-central"),
        "analyze",
        json!({"kind": "central", "top": 3}),
    );
    assert!(text.contains("  1. hub [Person]"), "{text}");
    assert!(
        text.contains("PageRank over every edge type, following edge direction, converged"),
        "{text}"
    );
    assert_eq!(report["listed"], json!(3), "{report}");
    assert_eq!(report["nodes"], json!(11), "{report}");
}

/// Binding: components are grouped, with sizes, never listed node by node.
#[test]
fn analyze_components_reports_groups_and_counts_singletons() {
    let (text, report) = task_both(
        analyze_store("analyze-wcc"),
        "analyze",
        json!({"kind": "components"}),
    );
    assert!(
        text.contains("6 component(s) over 11 node(s)")
            && text.contains("largest 5; 4 singleton(s)"),
        "{text}"
    );
    assert!(text.contains("  1. size 5 — hub, s1, s2, s3, s4"), "{text}");
    assert!(text.contains("  2. size 2 — p1, p2"), "{text}");
    assert_eq!(
        report["listed"],
        json!(2),
        "singletons are counted, not listed"
    );
}

/// Binding (R6): clusters drops singletons and says how many.
#[test]
fn analyze_clusters_drops_singletons_and_says_how_many() {
    let (text, report) = task_both(
        analyze_store("analyze-louvain"),
        "analyze",
        json!({"kind": "clusters"}),
    );
    assert!(text.contains("4 singleton(s) not listed"), "{text}");
    assert_eq!(report["singletons"], json!(4), "{report}");
    for row in report["rows"].as_array().unwrap() {
        assert!(row["size"].as_u64().unwrap() > 1, "{row}");
    }
}

/// Floats at fixed precision: the json reply rounds as the text does — 3
/// places for modularity, 2 for cohesion. Two triangles joined by one edge
/// score modularity 6/7 - 1/2 = 0.357142…, which has no exact decimal form.
#[test]
fn analyze_clusters_json_rounds_its_floats_as_the_text_does() {
    let db = memory_store("analyze-rounding");
    {
        let mut g = db.write();
        for key in ["a", "b", "c", "d", "e", "f"] {
            g.insert_node("Person", key, vec![("name".into(), Value::Str(key.into()))])
                .unwrap();
        }
        for (s, d) in [
            ("a", "b"),
            ("b", "c"),
            ("a", "c"),
            ("d", "e"),
            ("e", "f"),
            ("d", "f"),
            ("c", "d"),
        ] {
            g.insert_edge("KNOWS", s, d).unwrap();
        }
    }
    let report = task_report(db, "analyze", json!({"kind": "clusters"}));
    assert_eq!(report["modularity"], json!(0.357), "{report}");
    for row in report["rows"].as_array().unwrap() {
        let c = row["cohesion"].as_f64().unwrap();
        assert_eq!(c, (c * 100.0).round() / 100.0, "{row}");
    }
}

/// Binding (R6): no wall-clock budget, so the same store gets the same answer.
#[test]
fn analyze_answers_the_same_store_the_same_way_every_time() {
    let db = analyze_store("analyze-determinism");
    for kind in ["central", "clusters", "components", "degree"] {
        let first = task_reply(&one_task_call(db.clone(), "analyze", json!({"kind": kind})));
        let second = task_reply(&one_task_call(db.clone(), "analyze", json!({"kind": kind})));
        assert_eq!(first, second, "{kind}");
    }
}

#[test]
fn analyze_refuses_an_unknown_kind_and_names_real_edge_types() {
    let db = analyze_store("analyze-args");
    let err = error_text(&one_task_call(
        db.clone(),
        "analyze",
        json!({"kind": "vibes"}),
    ));
    assert!(
        err.contains("central, clusters, components, degree, identities"),
        "{err}"
    );
    let err = error_text(&one_task_call(
        db,
        "analyze",
        json!({"kind": "central", "edge_type": "NOPE"}),
    ));
    assert!(
        err.contains("no edge type named NOPE") && err.contains("KNOWS"),
        "{err}"
    );
}

/// Binding (R6): `top` is capped, whatever the caller asks for.
#[test]
fn analyze_lists_at_most_fifty_rows() {
    let db = memory_store("analyze-cap");
    {
        let mut g = db.write();
        for i in 0..120 {
            g.insert_node("Person", &format!("n{i:03}"), vec![])
                .unwrap();
        }
    }
    let report = task_report(db, "analyze", json!({"kind": "degree", "top": 500}));
    assert_eq!(report["listed"], json!(50), "{report}");
    assert_eq!(report["nodes"], json!(120), "{report}");
}

/// Review focus: on a 100,000-node store the four analyses and the rule
/// proposals still answer inside the reply's bounds, and in time.
///
/// Ignored because building the store is the slow part, not the tools:
/// `cargo test --release -p mushroomdb-server --test mcp -- --ignored
/// analyze_and_suggest_stay_bounded_on_a_100k_node_store`.
#[test]
#[ignore = "builds a 100,000-node store; run in release with --ignored"]
fn analyze_and_suggest_stay_bounded_on_a_100k_node_store() {
    const NODES: usize = 100_000;
    let db = open("scale-100k");
    {
        let mut g = db.write();
        let mut b = g.batch();
        for i in 0..NODES {
            b.insert_node(
                if i % 10 == 0 { "Person" } else { "Note" },
                &format!("n{i}"),
                vec![("team".into(), Value::Str(format!("t{}", i % 7)))],
            );
        }
        for i in 1..NODES {
            // A star: every node linked to one hub, the shape of a memory
            // store where every note is about the same person.
            b.insert_edge("ABOUT", &format!("n{i}"), "n0");
        }
        b.commit().unwrap();
    }
    for kind in ["central", "clusters", "components", "degree"] {
        let started = std::time::Instant::now();
        let text = task_reply(&one_task_call(
            db.clone(),
            "analyze",
            json!({"kind": kind, "top": 50}),
        ));
        let took = started.elapsed();
        assert!(
            text.lines().count() <= 51,
            "{kind}: {} lines",
            text.lines().count()
        );
        assert!(text.len() <= 16_000, "{kind}: {} bytes", text.len());
        assert!(took.as_secs() < 10, "{kind} took {took:?}");
        eprintln!("{kind}: {took:?}, {} bytes", text.len());
    }
    let started = std::time::Instant::now();
    let text = task_reply(&one_task_call(db.clone(), "suggest_rules", json!({})));
    let took = started.elapsed();
    assert!(text.len() <= 16_000, "suggest_rules: {} bytes", text.len());
    assert!(took.as_secs() < 10, "suggest_rules took {took:?}");
    eprintln!("suggest_rules: {took:?}, {} bytes", text.len());
}

// ─────────────────────────────────────────────────────────────────────────────
// identity — aliases, the preset's report, and resolution
// ─────────────────────────────────────────────────────────────────────────────

fn aliases_in(db: &SharedDb, key: &str) -> Vec<String> {
    match db.read().get_prop(key, "aliases") {
        Some(Value::List(items)) => items
            .into_iter()
            .filter_map(|v| match v {
                Value::Str(s) => Some(s),
                _ => None,
            })
            .collect(),
        other => panic!("{key} has no aliases list: {other:?}"),
    }
}

/// Binding (OD-1, as amended 2026-10-01): `upsert_entity` keeps the
/// normalised list from the key and the name, takes caller aliases as an
/// argument — kept in `alias_keys`, as written — and refuses them as a
/// property.
#[test]
fn upsert_entity_keeps_a_normalised_alias_list() {
    let db = memory_store("upsert-aliases");
    one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew-sherlin", "label": "Person",
               "props": {"name": "Matthew Sherlin"}, "aliases": ["Matt"]}),
    );
    assert_eq!(
        aliases_in(&db, "matthew-sherlin"),
        vec!["matthew", "matthew sherlin", "matthew-sherlin", "sherlin"]
    );
    assert_eq!(
        db.read().get_prop("matthew-sherlin", "alias_keys"),
        Some(Value::List(vec![Value::Str("Matt".into())]))
    );
    let err = error_text(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew-sherlin", "props": {"aliases": ["m"]}}),
    ));
    assert!(err.contains("maintained by the store"), "{err}");
    let err = error_text(&one_task_call(
        db,
        "upsert_entity",
        json!({"key": "matthew-sherlin", "props": {}, "aliases": "Matt"}),
    ));
    assert!(err.contains("aliases must be an array of strings"), "{err}");
}

/// Binding (OD-1): `remember`'s entities take aliases too, and stubs get the
/// list their key implies.
#[test]
fn remember_entities_and_stubs_carry_aliases() {
    let db = memory_store("remember-aliases");
    one_task_call(
        db.clone(),
        "remember",
        json!({
            "text": "Matt and Reid met",
            "about": ["reid"],
            "entities": [{"key": "matthew", "label": "Person", "aliases": ["Matt"]}]
        }),
    );
    assert_eq!(aliases_in(&db, "matthew"), vec!["matthew"]);
    assert_eq!(
        db.read().get_prop("matthew", "alias_keys"),
        Some(Value::List(vec![Value::Str("Matt".into())]))
    );
    assert_eq!(aliases_in(&db, "reid"), vec!["reid"]);
    let err = error_text(&one_task_call(
        db,
        "remember",
        json!({"text": "x", "entities": [{"key": "k", "label": "Person", "aliases": 3}]}),
    ));
    assert!(
        err.contains("entities[].aliases must be an array of strings"),
        "{err}"
    );
}

/// A memory store with the identity preset applied, as
/// `schema apply --memory-identity` leaves one.
fn identity_store(name: &str) -> SharedDb {
    let db = memory_store(name);
    db.write()
        .apply_schema(&core_api::memory_schema::memory_identity())
        .unwrap();
    db
}

/// Binding (R2): with the preset on, `remember` says which identities its
/// write agreed with — once per pair, though the rule derived both directions.
#[test]
fn remember_reports_the_same_as_claims_it_created() {
    let db = identity_store("remember-same-as");
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "first", "entities": [{"key": "matthew-sherlin", "label": "Person",
                                              "props": {"name": "Matthew Sherlin"}}]}),
    );
    let args = json!({"text": "second", "entities": [{"key": "msherlin", "label": "Person",
                                                    "props": {"name": "Matthew Sherlin"}}]});
    let text = task_reply(&one_task_call(db.clone(), "remember", args));
    assert_eq!(
        text.matches("same as  matthew-sherlin ~ msherlin (0.60)")
            .count(),
        1,
        "{text}"
    );
    let report = task_report(
        db,
        "remember",
        json!({"text": "third", "entities": [{"key": "m-sherlin", "label": "Person",
                                              "props": {"name": "Matthew Sherlin"}}]}),
    );
    assert_eq!(
        report["same_as"].as_array().map(Vec::len),
        Some(2),
        "the third links to both earlier ones: {report}"
    );
}

/// Determinism: a `same_as` score in the json report is at fixed precision.
/// A three-word name shared under two keys is four aliases out of six, 2/3,
/// which has no finite decimal form.
#[test]
fn remember_json_same_as_scores_are_two_decimal_places() {
    let db = identity_store("remember-same-as-precision");
    let props = json!({"name": "P Q R"});
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "first", "entities": [{"key": "ka", "label": "Person",
                                              "props": props}]}),
    );
    let report = task_report(
        db,
        "remember",
        json!({"text": "second", "entities": [{"key": "kb", "label": "Person",
                                               "props": props}]}),
    );
    assert_eq!(
        report["same_as"],
        json!([{"a": "ka", "b": "kb", "score": 0.67}]),
        "{report}"
    );
}

fn alias_keys_in(db: &SharedDb, key: &str) -> Option<Value> {
    db.read().get_prop(key, "alias_keys")
}

fn str_list(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

/// Owner decision Q2: an entity that declares an alias links the stub keyed
/// exactly so, and `remember` reports the link — whichever was written first.
#[test]
fn remember_reports_a_stub_linked_by_a_declared_alias() {
    let entity = json!({"key": "matthew-sherlin", "label": "Person",
                        "props": {"name": "Matthew Sherlin"}, "aliases": ["matt"]});
    let line = "same as  matt ~ matthew-sherlin (1.00) — explain_association shows why";

    let db = identity_store("claim-stub-first");
    let text = task_reply(&one_task_call(
        db.clone(),
        "remember",
        json!({"text": "Matt owns 0.7", "about": ["matt"]}),
    ));
    assert!(!text.contains("same as"), "nothing to link yet: {text}");
    let text = task_reply(&one_task_call(
        db.clone(),
        "remember",
        json!({"text": "Matthew Sherlin is Matt", "entities": [entity.clone()]}),
    ));
    assert_eq!(text.matches(line).count(), 1, "{text}");
    assert_eq!(
        alias_keys_in(&db, "matthew-sherlin"),
        Some(str_list(&["matt"]))
    );
    // The line's own promise: explain_association does show why.
    let why = task_reply(&one_task_call(
        db,
        "explain_association",
        json!({"a": "matthew-sherlin", "b": "matt"}),
    ));
    assert!(why.contains("same_as_claim_person"), "{why}");

    let db = identity_store("claim-entity-first");
    one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew-sherlin", "label": "Person",
               "props": {"name": "Matthew Sherlin"}, "aliases": ["matt"]}),
    );
    let report = task_report(
        db,
        "remember",
        json!({"text": "Matt owns 0.7", "about": ["matt"]}),
    );
    assert_eq!(
        report["same_as"],
        json!([{"a": "matt", "b": "matthew-sherlin", "score": 1.0}]),
        "{report}"
    );
}

/// `alias_keys` is the store's, like `aliases`: refused as a property on both
/// write tools, with the argument to use named.
#[test]
fn alias_keys_is_refused_as_a_property() {
    let db = memory_store("alias-keys-prop");
    for (tool, args) in [
        (
            "upsert_entity",
            json!({"key": "m", "label": "Person", "props": {"alias_keys": ["matt"]}}),
        ),
        (
            "remember",
            json!({"text": "x", "entities": [{"key": "m", "label": "Person",
                                             "props": {"alias_keys": ["matt"]}}]}),
        ),
    ] {
        let err = error_text(&one_task_call(db.clone(), tool, args));
        assert!(
            err.contains("'alias_keys' is maintained by the store")
                && err.contains("'aliases' argument"),
            "{tool}: {err}"
        );
    }
    assert!(!db.read().has_node("m"), "a refusal writes nothing");
}

/// Defect 76: a key with nothing in it is refused by both write tools, as a
/// tool error naming the argument, and `stats` reads the same before and
/// after. The check is `core-api`'s; this pins that the tools surface it.
#[test]
fn an_empty_key_is_a_tool_error_and_writes_nothing() {
    let db = memory_store("empty-key");
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "Ada wrote the first one", "about": ["ada"]}),
    );
    let before = content_json(&one_task_call(db.clone(), "stats", json!({})));
    for (tool, args, argument) in [
        (
            "remember",
            json!({"text": "about nothing", "about": ["ada", ""]}),
            "remember: about[1]",
        ),
        (
            "remember",
            json!({"text": "about a blank", "about": ["   "]}),
            "remember: about[0]",
        ),
        (
            "remember",
            json!({"text": "a fact from nothing",
                   "facts": [{"subject": "", "predicate": "KNOWS", "object": "ada"}]}),
            "remember: facts[0].subject",
        ),
        (
            "remember",
            json!({"text": "a fact to nothing",
                   "facts": [{"subject": "ada", "predicate": "KNOWS", "object": ""}]}),
            "remember: facts[0].object",
        ),
        (
            "remember",
            json!({"text": "an entity with no key",
                   "entities": [{"key": "", "label": "Person"}]}),
            "remember: entities[0].key",
        ),
        (
            "upsert_entity",
            json!({"key": "", "label": "Person", "props": {"name": "Nobody"}}),
            "key",
        ),
    ] {
        let err = error_text(&one_task_call(db.clone(), tool, args));
        assert!(
            err.contains(&format!("{argument} must not be empty or only whitespace")),
            "{tool} {argument}: {err}"
        );
    }
    assert_eq!(
        content_json(&one_task_call(db.clone(), "stats", json!({}))),
        before,
        "a refusal writes nothing"
    );
    assert!(!db.read().has_node(""), "no node keyed by the empty string");
}

/// Defect 77: a fact's `predicate` and an entity's `label` with nothing in
/// them are tool errors naming the argument, and `stats` and `schema` read the
/// same before and after — no unnamed edge type, no unnamed label, no
/// full-text pair declared for one.
#[test]
fn an_empty_predicate_or_label_is_a_tool_error_and_writes_nothing() {
    let db = memory_store("empty-type");
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "Ada wrote the first one", "about": ["ada"]}),
    );
    let snapshot = |db: &SharedDb| {
        (
            content_json(&one_task_call(db.clone(), "stats", json!({}))),
            task_reply(&one_task_call(db.clone(), "schema", json!({}))),
        )
    };
    let before = snapshot(&db);
    for (tool, args, argument) in [
        (
            "remember",
            json!({"text": "a fact with no predicate",
                   "entities": [{"key": "v0.7", "label": "Release"}],
                   "facts": [{"subject": "ada", "predicate": "", "object": "v0.7"}]}),
            "remember: facts[0].predicate",
        ),
        (
            "remember",
            json!({"text": "an entity with no label",
                   "entities": [{"key": "v0.7", "label": "Release"},
                                {"key": "widget", "label": "  "}]}),
            "remember: entities[1].label",
        ),
        (
            "upsert_entity",
            json!({"key": "gizmo", "label": "", "props": {"name": "Gizmo"}}),
            "label",
        ),
    ] {
        let err = error_text(&one_task_call(db.clone(), tool, args));
        assert!(
            err.contains(&format!("{argument} must not be empty or only whitespace")),
            "{tool} {argument}: {err}"
        );
    }
    assert_eq!(snapshot(&db), before, "a refusal writes nothing");
    let g = db.read();
    for key in ["v0.7", "widget", "gizmo"] {
        assert!(!g.has_node(key), "{key} must not have been written");
    }
}

/// `forget {key, prop: "alias_keys"}` clears the declared list, and with it
/// the link the claim made; the reply counts the retraction.
#[test]
fn forgetting_alias_keys_clears_the_claim() {
    let db = identity_store("forget-alias-keys");
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "Matt", "about": ["matt"],
               "entities": [{"key": "matthew-sherlin", "label": "Person",
                             "props": {"name": "Matthew Sherlin"}, "aliases": ["matt"]}]}),
    );
    let same_as = |db: &SharedDb| db.read().weighted_edges("SAME_AS", None).len();
    assert_eq!(same_as(&db), 1, "precondition: the claim linked them");

    let text = task_reply(&one_task_call(
        db.clone(),
        "forget",
        json!({"key": "matthew-sherlin", "prop": "alias_keys"}),
    ));
    assert!(
        text.starts_with("forgot matthew-sherlin.alias_keys"),
        "{text}"
    );
    assert!(
        text.contains("1 derived edge(s) retracted because a rule read alias_keys"),
        "{text}"
    );
    assert_eq!(alias_keys_in(&db, "matthew-sherlin"), None);
    assert_eq!(same_as(&db), 0);

    // A later write that declares nothing does not bring the claim back.
    one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew-sherlin", "props": {"role": "owner"}}),
    );
    assert_eq!(alias_keys_in(&db, "matthew-sherlin"), None);
    assert_eq!(same_as(&db), 0);
}

/// Forgetting `aliases` leaves the declared list, which still links a stub
/// keyed so. The reply says that, and how to clear it — and says nothing of
/// the kind for a node that declared no alias.
#[test]
fn forgetting_aliases_says_declared_aliases_remain_in_alias_keys() {
    let db = identity_store("forget-aliases-keys-remain");
    let person = |key: &str, name: &str, aliases: Js| json!({"key": key, "label": "Person", "props": {"name": name}, "aliases": aliases});
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "Matt, Grace and Reid", "about": ["matt"],
               "entities": [person("matthew-sherlin", "Matthew Sherlin", json!(["matt"])),
                            person("grace-hopper", "Grace Hopper", json!(["amazing grace"])),
                            person("reid", "Reid Hoffman", json!([]))]}),
    );
    let forget_aliases = |key: &str| json!({"key": key, "prop": "aliases"});

    let text = task_reply(&one_task_call(db.clone(), "forget", forget_aliases("reid")));
    assert!(text.starts_with("forgot reid.aliases"), "{text}");
    assert!(
        !text.contains("alias_keys"),
        "reid declared nothing: {text}"
    );

    let text = task_reply(&one_task_call(
        db.clone(),
        "forget",
        forget_aliases("matthew-sherlin"),
    ));
    assert!(text.starts_with("forgot matthew-sherlin.aliases"), "{text}");
    assert!(
        text.contains("its declared aliases remain in `alias_keys`")
            && text.contains("forget {key: \"matthew-sherlin\", prop: \"alias_keys\"}"),
        "{text}"
    );
    assert_eq!(
        alias_keys_in(&db, "matthew-sherlin"),
        Some(str_list(&["matt"])),
        "the reply is true: the list is still there"
    );
    assert_eq!(
        db.read().weighted_edges("SAME_AS", None).len(),
        1,
        "and so is the link it makes"
    );

    let report = task_report(db.clone(), "forget", forget_aliases("grace-hopper"));
    assert_eq!(report["changed"], json!(true), "{report}");
    assert_eq!(report["alias_keys_remain"], json!(true), "{report}");
    let report = task_report(db, "forget", forget_aliases("grace-hopper"));
    assert_eq!(report["changed"], json!(false), "{report}");
    assert_eq!(
        report["alias_keys_remain"],
        json!(false),
        "nothing was forgotten, so nothing is said to remain: {report}"
    );
}

/// The claim's edge is rule-owned like any other: retracting it by hand is
/// refused, naming the rule that owns that direction and the field it reads.
#[test]
fn forgetting_a_claimed_same_as_fact_names_the_claim_rule() {
    let db = identity_store("forget-claim-fact");
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "Matt", "about": ["matt"],
               "entities": [{"key": "matthew-sherlin", "label": "Person",
                             "props": {"name": "Matthew Sherlin"}, "aliases": ["matt"]}]}),
    );
    let err = error_text(&one_task_call(
        db.clone(),
        "forget",
        json!({"fact": {"subject": "matthew-sherlin", "predicate": "SAME_AS", "object": "matt"}}),
    ));
    assert!(
        err.contains("derived by rule same_as_claim_person") && err.contains("(alias_keys)"),
        "{err}"
    );
    assert!(!err.contains("same_as_entity_person"), "{err}");
    assert_eq!(db.read().weighted_edges("SAME_AS", None).len(), 1);
}

/// Two same-named people, linked at 3/5 — exactly the floor.
fn linked_pair_store(name: &str) -> SharedDb {
    let db = identity_store(name);
    for key in ["matthew-sherlin", "msherlin"] {
        one_task_call(
            db.clone(),
            "upsert_entity",
            json!({"key": key, "label": "Person", "props": {"name": "Matthew Sherlin"}}),
        );
    }
    assert_eq!(
        db.read().weighted_edges("SAME_AS", None).len(),
        2,
        "precondition: linked, both directions"
    );
    db
}

/// Owner decision, 2026-10-01: declaring an alias on one of two linked
/// entities costs nothing. The link stays, and neither tool reports a loss.
#[test]
fn declaring_an_alias_on_one_of_two_linked_entities_keeps_the_link() {
    let db = linked_pair_store("declared-keeps");
    let (text, report) = task_both(
        db.clone(),
        "remember",
        json!({"text": "Matthew goes by Matt",
               "entities": [{"key": "matthew-sherlin", "label": "Person",
                             "aliases": ["matt"]}]}),
    );
    assert!(!text.contains("unlinked"), "{text}");
    assert_eq!(report["same_as_lost_total"], json!(0), "{report}");
    let reply = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "msherlin", "props": {}, "aliases": ["sherl"]}),
    ));
    assert!(reply.get("same_as_lost").is_none(), "{reply}");
    assert_eq!(db.read().weighted_edges("SAME_AS", None).len(), 2);
}

/// Controller ruling: the reply is the contract. What a describing write can
/// still retract is a link a name made: renaming one of two linked entities
/// takes their overlap below the floor; `remember` says so, in the text and
/// in the json.
#[test]
fn remember_reports_the_same_as_link_a_rename_costs() {
    let args = json!({"text": "Matthew is Matt S now",
                      "entities": [{"key": "matthew-sherlin", "label": "Person",
                                    "props": {"name": "Matt S"}}]});
    let text = task_reply(&one_task_call(
        linked_pair_store("lost-text"),
        "remember",
        args.clone(),
    ));
    let line = text
        .lines()
        .find(|l| l.starts_with("unlinked"))
        .unwrap_or_else(|| panic!("no unlinked line: {text}"));
    assert!(
        line.starts_with(
            "unlinked  1 same-as link(s) this write retracted: matthew-sherlin ~ msherlin (was 0.60) — "
        ),
        "{line}"
    );
    assert!(
        line.ends_with(
            " — a link holds while two nodes' keys, names and the names' words overlap at \
             0.6, and this write changed them; give both the same name, or declare a \
             provisional stub's key as an alias to link it whatever the names"
        ),
        "the cause and the remedy: {line}"
    );
    assert_eq!(text.matches("unlinked").count(), 1, "one line: {text}");

    let db = linked_pair_store("lost-json");
    let report = task_report(db.clone(), "remember", args);
    assert_eq!(
        report["same_as_lost"],
        json!([{"a": "matthew-sherlin", "b": "msherlin", "score": 0.6}]),
        "{report}"
    );
    assert_eq!(report["same_as_lost_total"], json!(1), "{report}");
    assert_eq!(
        db.read().weighted_edges("SAME_AS", None).len(),
        0,
        "the reply is true"
    );
}

/// `upsert_entity` retracts the same way and says so the same way: a stub
/// spelling the full name keeps its link when the entity declares a nickname,
/// and loses it when the entity is renamed.
#[test]
fn upsert_entity_reports_the_full_name_stub_a_rename_unlinks() {
    let db = identity_store("lost-upsert-stub");
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "named first", "about": ["Matthew_Sherlin"]}),
    );
    let reply = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew-sherlin", "label": "Person",
               "props": {"name": "Matthew Sherlin"}}),
    ));
    assert!(
        reply.get("same_as_lost").is_none(),
        "a create loses nothing: {reply}"
    );
    assert_eq!(db.read().weighted_edges("SAME_AS", None).len(), 1);

    let reply = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew-sherlin", "props": {}, "aliases": ["matt"]}),
    ));
    assert!(
        reply.get("same_as_lost").is_none(),
        "a declared alias loses nothing: {reply}"
    );
    assert_eq!(db.read().weighted_edges("SAME_AS", None).len(), 1);

    let reply = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew-sherlin", "props": {"name": "Matt S"}}),
    ));
    assert_eq!(
        reply["same_as_lost"],
        json!([{"a": "Matthew_Sherlin", "b": "matthew-sherlin", "score": 0.6}]),
        "{reply}"
    );
    assert_eq!(reply["same_as_lost_total"], json!(1), "{reply}");
    assert!(
        reply["same_as_lost_note"].as_str().is_some_and(|n| n
            .starts_with("this write retracted 1 SAME_AS link(s): a link holds while")
            && n.contains("give both the same name")
            && n.contains("declare a provisional stub's key as an alias")),
        "{reply}"
    );
    assert_eq!(db.read().weighted_edges("SAME_AS", None).len(), 0);
}

/// A write that retracts nothing says nothing about it: no text line, an
/// empty list from `remember`, and no field at all from `upsert_entity`.
#[test]
fn a_write_that_loses_no_link_reports_none() {
    let db = linked_pair_store("lost-none");
    let (text, report) = task_both(
        db.clone(),
        "remember",
        json!({"text": "both go by Matt",
               "entities": [{"key": "matthew-sherlin", "label": "Person", "aliases": ["matt"]},
                            {"key": "msherlin", "label": "Person", "aliases": ["matt"]}]}),
    );
    assert!(!text.contains("unlinked"), "{text}");
    assert_eq!(report["same_as_lost"], json!([]), "{report}");
    assert_eq!(report["same_as_lost_total"], json!(0), "{report}");
    let reply = content_json(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "msherlin", "props": {"role": "owner"}}),
    ));
    assert_eq!(
        reply,
        json!({"ok": true, "key": "msherlin", "label": "Person",
               "created": false, "updated_fields": 1}),
        "unchanged when nothing is lost"
    );
    assert_eq!(db.read().weighted_edges("SAME_AS", None).len(), 2);
}

/// The lost list is bounded: ten pairs, and a count of the rest.
#[test]
fn the_lost_links_are_listed_up_to_ten_and_counted() {
    let store = |name: &str| {
        let db = identity_store(name);
        for i in 0..12 {
            one_task_call(
                db.clone(),
                "upsert_entity",
                json!({"key": format!("k{i:02}"), "label": "Person",
                       "props": {"name": "Matthew Sherlin"}}),
            );
        }
        db
    };
    let args = json!({"text": "k00 is Matt S now",
                      "entities": [{"key": "k00", "label": "Person",
                                    "props": {"name": "Matt S"}}]});
    let report = task_report(store("lost-cap-json"), "remember", args.clone());
    assert_eq!(report["same_as_lost_total"], json!(11), "{report}");
    assert_eq!(report["same_as_lost"].as_array().map(Vec::len), Some(10));
    let text = task_reply(&one_task_call(store("lost-cap-text"), "remember", args));
    let line = text
        .lines()
        .find(|l| l.starts_with("unlinked"))
        .expect("line");
    assert!(
        line.starts_with(
            "unlinked  11 same-as link(s) this write retracted: k00 ~ k01 (was 0.60), "
        ),
        "{line}"
    );
    assert_eq!(line.matches(" ~ ").count(), 10, "{line}");
    assert!(line.contains("k00 ~ k10 (was 0.60) (+1 more) — "), "{line}");

    let reply = content_json(&one_task_call(
        store("lost-cap-upsert"),
        "upsert_entity",
        json!({"key": "k00", "props": {"name": "Matt S"}}),
    ));
    assert_eq!(reply["same_as_lost_total"], json!(11), "{reply}");
    assert_eq!(reply["same_as_lost"].as_array().map(Vec::len), Some(10));
}

/// Important 1, at the tool surface. A subject named by `about` before it was
/// described is an `Entity` for life: `upsert_entity` refuses to relabel it,
/// `remember`'s `entities` describes it without relabelling, and either way
/// the alias it declares is stored and links nothing — the claim rules run
/// from the five entity labels. An `Entity→Entity` rule is the owner's call.
#[test]
fn a_subject_named_before_it_was_described_cannot_claim_a_stub() {
    let db = identity_store("ex-stub-cannot-claim");
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "named first", "about": ["matthew-sherlin"]}),
    );
    let err = error_text(&one_task_call(
        db.clone(),
        "upsert_entity",
        json!({"key": "matthew-sherlin", "label": "Person",
               "props": {"name": "Matthew Sherlin"}, "aliases": ["matt"]}),
    ));
    assert!(
        err.contains("exists as \"Entity\", not \"Person\""),
        "{err}"
    );
    one_task_call(
        db.clone(),
        "remember",
        json!({"text": "described", "entities": [{"key": "matthew-sherlin", "label": "Person",
               "props": {"name": "Matthew Sherlin"}, "aliases": ["matt"]}]}),
    );
    assert_eq!(
        db.read()
            .node_ref("matthew-sherlin")
            .map(|n| n.label().to_string()),
        Some("Entity".to_string())
    );
    assert_eq!(
        alias_keys_in(&db, "matthew-sherlin"),
        Some(str_list(&["matt"]))
    );
    let report = task_report(
        db.clone(),
        "remember",
        json!({"text": "the nickname", "about": ["matt"]}),
    );
    assert_eq!(report["same_as"], json!([]), "{report}");
    assert_eq!(db.read().weighted_edges("SAME_AS", None).len(), 0);
}

/// `alias_keys` is bookkeeping. Twelve people who all declare one alias give
/// the engine a list field with a sampled Jaccard of 1.0 — exactly what it
/// proposes an `Overlap` rule over — and the reply must not relay it.
#[test]
fn suggest_rules_never_proposes_a_rule_on_alias_keys() {
    let db = memory_store("suggest-alias-keys");
    for i in 0..12 {
        one_task_call(
            db.clone(),
            "upsert_entity",
            json!({"key": format!("p{i}"), "label": "Person",
                   "props": {"name": format!("Person {i}")},
                   "aliases": ["crew"]}),
        );
    }
    let (text, report) = task_both(db, "suggest_rules", json!({}));
    for s in report["suggestions"].as_array().expect("suggestions") {
        assert!(
            !s["create_rule_args"]
                .to_string()
                .contains("\"field\":\"alias_keys\""),
            "proposed a rule over alias_keys: {s}"
        );
    }
    assert!(!text.contains("OVERLAPS_ALIAS_KEYS"), "{text}");
    assert!(
        text.contains("alias_keys — not shown"),
        "the hidden fields are named: {text}"
    );
}

/// Binding (OD-2): `suggest_rules` offers the preset to a store with entities
/// and no SAME_AS rule, as a command — and stops once the preset is on.
#[test]
fn suggest_rules_offers_the_identity_preset_until_it_is_applied() {
    let db = memory_store("suggest-identity");
    seed_person(&db, "matthew");
    let (text, report) = task_both(db.clone(), "suggest_rules", json!({}));
    assert!(text.contains("--memory-identity"), "{text}");
    assert!(text.contains("not a create_rule call"), "{text}");
    assert_eq!(
        report["identity_preset"]["entity_nodes"],
        json!(1),
        "{report}"
    );
    db.write()
        .apply_schema(&core_api::memory_schema::memory_identity())
        .unwrap();
    let report = task_report(db, "suggest_rules", json!({}));
    assert!(report["identity_preset"].is_null(), "{report}");
}

/// Determinism: a proposal's example scores are at fixed precision in the
/// json report too, not only in the rendered text — a Jaccard of 1/3 is
/// `0.33`, never `0.3333333333333333`.
#[test]
fn suggest_rules_json_example_scores_are_two_decimal_places() {
    let db = memory_store("suggest-precision");
    {
        let mut g = db.write();
        for i in 0..6 {
            for (label, own) in [("Red", "r"), ("Blue", "b")] {
                g.insert_node(
                    label,
                    &format!("{own}{i}"),
                    vec![(
                        "tags".into(),
                        Value::List(vec![
                            Value::Str("shared".into()),
                            Value::Str(format!("{own}{i}")),
                        ]),
                    )],
                )
                .unwrap();
            }
        }
    }
    let report = task_report(db, "suggest_rules", json!({}));
    let scores: Vec<f64> = report["suggestions"]
        .as_array()
        .expect("suggestions")
        .iter()
        .flat_map(|s| s["examples"].as_array().cloned().unwrap_or_default())
        .map(|e| e[2].as_f64().expect("score"))
        .collect();
    assert!(
        !scores.is_empty(),
        "the fixture must yield examples: {report}"
    );
    for score in scores {
        assert_eq!(
            score,
            (score * 100.0).round() / 100.0,
            "an example score past two decimal places: {report}"
        );
    }
}

/// Binding (OD-3): `analyze` resolves SAME_AS into identities — every pair
/// linked, the oldest node first — and takes no edge type for it.
#[test]
fn analyze_identities_lists_each_identity_under_its_oldest_node() {
    let db = identity_store("analyze-identities");
    for (i, key) in ["matthew-sherlin", "msherlin"].iter().enumerate() {
        one_task_call(
            db.clone(),
            "remember",
            json!({"text": format!("mention {i}"),
                   "entities": [{"key": key, "label": "Person",
                                 "props": {"name": "Matthew James Sherlin"}}]}),
        );
    }
    let (text, report) = task_both(db.clone(), "analyze", json!({"kind": "identities"}));
    assert!(
        text.contains("1 identit(ies) over 2 linked node(s), 1 SAME_AS claim(s) at ≥ 0.6"),
        "{text}"
    );
    assert!(
        text.contains("  1. matthew-sherlin — matthew-sherlin, msherlin, weakest link 0.67"),
        "{text}"
    );
    assert_eq!(report["rows"][0]["canonical"], json!("matthew-sherlin"));
    // A three-word name makes the score 4/6: the reply carries it at two
    // places, as `same_as` does, not as 0.6666666666666666.
    assert_eq!(report["rows"][0]["weakest"], json!(0.67), "{report}");
    let err = error_text(&one_task_call(
        db,
        "analyze",
        json!({"kind": "identities", "edge_type": "SAME_AS"}),
    ));
    assert!(
        err.contains("edge_type does not apply to identities"),
        "{err}"
    );
}

/// Binding: `rename_node` moves the `id` that `upsert_entity` stored, so the
/// two ways of naming a node in Cypher keep agreeing (ledger row 72).
///
/// `upsert_entity`'s create path stores the key as an `id` property. Until
/// 0.7.1 a rename left it at the old key: `WHERE n.id = '<new key>'` found
/// nothing, and `RETURN n.id` printed a key the node no longer had.
#[test]
fn rename_node_moves_the_id_upsert_entity_stored() {
    let stdin = format!(
        "{}{}{}{}{}",
        call(
            1,
            "upsert_entity",
            json!({"key": "q1", "label": "Person", "props": {"name": "Quinn"}})
        ),
        call(2, "rename_node", json!({"old_key": "q1", "new_key": "q2"})),
        call(3, "node_info", json!({"key": "q2"})),
        call(
            4,
            "query",
            json!({"cypher": "MATCH (n:Person) WHERE n.id = 'q2' RETURN key(n), n.id"})
        ),
        call(
            5,
            "query",
            json!({"cypher": "MATCH (n:Person) WHERE n.id = 'q1' RETURN key(n), n.id"})
        ),
    );
    let (res, out) = exchange(open("rename-id"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);

    assert_eq!(content_json(&replies[1])["ok"], json!(true));
    let node = content_json(&replies[2]);
    assert_eq!(node["props"]["id"], json!("q2"), "{node}");
    assert_eq!(
        content_json(&replies[3])["rows"],
        json!([["q2", "q2"]]),
        "the new key finds the node, and `n.id` prints it"
    );
    assert_eq!(
        content_json(&replies[4])["rows"],
        json!([]),
        "nothing answers to the old key"
    );
}

/// Binding: `was_linked` declares the date its handler takes (ledger row 71).
///
/// The handler has resolved an RFC 3339 string since `edges_at` did, and the
/// skill teaches that form. While the schema said `integer`, a host that
/// checks arguments against `inputSchema` refused the call the skill teaches.
/// The two time-travel tools type their instant the same way, so the schema
/// is compared to `edges_at`'s rather than restated.
#[test]
fn was_linked_declares_the_date_it_accepts() {
    let db = open("was-linked-date");
    seed_person(&db, "alice");
    seed_person(&db, "bob");
    db.write().insert_edge("LINK", "alice", "bob").unwrap();
    let last = db.read().wal_total_commits().unwrap() - 1;

    let link = |at: Js| json!({"a": "alice", "b": "bob", "edge_type": "LINK", "at_commit": at});
    let stdin = format!(
        "{}{}{}{}{}",
        req(json!(1), "tools/list", None),
        call(2, "was_linked", link(json!("2099-01-01T00:00:00Z"))),
        call(3, "was_linked", link(json!(last))),
        call(4, "was_linked", link(json!("not a date"))),
        call(5, "was_linked", link(json!(1.5))),
    );
    let (res, out) = exchange(db, &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);

    let tools = replies[0]["result"]["tools"].as_array().expect("tools");
    let arg = |tool: &str, name: &str| -> Js {
        tools
            .iter()
            .find(|t| t["name"] == tool)
            .unwrap_or_else(|| panic!("{tool} is not listed"))["inputSchema"]["properties"][name]
            .clone()
    };
    let at_commit = arg("was_linked", "at_commit");
    assert_eq!(
        at_commit["anyOf"],
        json!([
            { "type": "string", "minLength": 1 },
            { "type": "integer", "minimum": 0 }
        ]),
        "a date string or a commit index: {at_commit}"
    );
    assert_eq!(
        at_commit["anyOf"],
        arg("edges_at", "at")["anyOf"],
        "`was_linked` and `edges_at` type their instant alike"
    );
    assert!(
        at_commit.get("type").is_none(),
        "a bare `type` beside `anyOf` would still refuse the string: {at_commit}"
    );
    assert!(
        at_commit["description"]
            .as_str()
            .is_some_and(|d| d.contains("RFC 3339")),
        "{at_commit}"
    );

    // What the schema now says is what the handler does.
    let by_date = content_json(&replies[1]);
    assert_eq!(by_date["linked"], json!(true), "{by_date}");
    assert_eq!(by_date["at_commit"], json!(last), "{by_date}");
    assert_eq!(content_json(&replies[2])["linked"], json!(true));
    assert!(
        replies[3]["result"]["isError"].as_bool().unwrap_or(false),
        "a string that is no date is a tool error, never a guessed commit: {}",
        replies[3]
    );
    assert!(
        error_text(&replies[4]).contains("non-negative commit index or an RFC 3339 date"),
        "{}",
        replies[4]
    );
}
