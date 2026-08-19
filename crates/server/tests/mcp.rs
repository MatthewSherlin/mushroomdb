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

use core_api::{SharedDb, Value};
use serde_json::{json, Value as Js};
use server::run_mcp_stdio;
use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-mcp-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn open(name: &str) -> SharedDb {
    SharedDb::open(&tmp(name)).unwrap()
}

fn exchange(db: SharedDb, stdin: &str) -> (std::io::Result<()>, Vec<u8>) {
    let mut reader = Cursor::new(stdin.as_bytes().to_vec());
    let mut writer = Cursor::new(Vec::new());
    let res = run_mcp_stdio(db, &mut reader, &mut writer);
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
            "serverInfo": {"name": "mushroomdb"}
        })
    );
}

/// Binding: tools/list returns exactly the eight tools with the specified schemas.
#[test]
fn tools_list_returns_eight_tools_with_schemas() {
    let stdin = req(json!(1), "tools/list", None);
    let (res, out) = exchange(open("list"), &stdin);
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
    assert_eq!(
        names,
        BTreeSet::from([
            "query",
            "ingest_json",
            "explain",
            "stats",
            "neighborhood",
            "node_info",
            "node_edges",
            "create_rule",
        ])
    );
    assert_eq!(tools.len(), 8);

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
    assert_eq!(edges["inputSchema"]["required"], json!(["key"]));

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
                "depth": 1,
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

/// Binding: node_info / node_edges MCP payloads match the HTTP wire shapes.
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
        call(2, "node_edges", json!({"key": "p1"})),
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
            "edges": [
                {
                    "edge_type": "KNOWS",
                    "src_key": "p1",
                    "dst_key": "p2",
                    "derived": false
                },
                {
                    "edge_type": "WORKS_AT",
                    "src_key": "p1",
                    "dst_key": "acme",
                    "derived": true
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
    assert_eq!(expl[0]["weight"], Js::Null);
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
    let res = run_mcp_stdio(open("eof"), &mut reader, &mut writer);
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
