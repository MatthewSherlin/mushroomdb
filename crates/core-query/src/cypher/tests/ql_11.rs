//! QL-11: `EXPLAIN` renders the plan of any statement as JSON without running
//! it, naming the index each lookup reads.

use crate::cypher::explain::{
    explain_plan, explain_plan_with, explain_prefix, explain_statement, ExplainPrefix, IndexNames,
};
use crate::cypher::{lex, parse_read, plan};

/// Index names a test declares: `eq_L_f` for equality, `idx_L_f` for range.
struct Declared;

impl IndexNames for Declared {
    fn equality(&self, label: &str, field: &str) -> Option<String> {
        Some(format!("eq_{label}_{field}"))
    }
    fn range(&self, label: &str, field: &str) -> Option<String> {
        Some(format!("idx_{label}_{field}"))
    }
}

fn explain(q: &str) -> String {
    let toks = lex(q).unwrap();
    explain_statement(&toks, &Declared).unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn explain_bare(q: &str) -> String {
    let toks = lex(q).unwrap();
    explain_statement(&toks, &crate::cypher::explain::NoIndexes).unwrap()
}

#[test]
fn a_read_plan_is_a_json_array_starting_at_the_scan() {
    let json = explain_bare("MATCH (n:Person) RETURN n.name");
    assert!(json.starts_with(r#"[{"op":"ScanLabel""#), "{json}");
    assert!(json.contains(r#""label":"Person""#), "{json}");
    assert!(json.contains(r#""op":"Project""#), "{json}");
    assert!(json.ends_with("]"), "{json}");
}

#[test]
fn explain_plan_serialises_the_planner_ops() {
    let q = parse_read(&lex("MATCH (n) RETURN count(n)").unwrap()).unwrap();
    let ops = plan(&q.parts[0]).unwrap();
    let json = explain_plan(&ops);
    assert!(json.contains(r#""op":"Aggregate""#), "{json}");
    assert!(json.contains(r#""func":"Count""#), "{json}");
    assert_eq!(
        json,
        explain_plan_with(&ops, &crate::cypher::explain::NoIndexes)
    );
}

#[test]
fn an_aggregate_names_its_op() {
    let json = explain_bare("MATCH (n) RETURN count(n)");
    assert!(json.contains(r#""op":"Aggregate""#), "{json}");
}

#[test]
fn an_equality_lookup_names_its_index_or_null() {
    let json = explain("MATCH (n:Person {name: 'a'}) RETURN n");
    assert!(json.contains(r#""op":"IndexScan""#), "{json}");
    assert!(json.contains(r#""index":"eq_Person_name""#), "{json}");
    let bare = explain_bare("MATCH (n:Person {name: 'a'}) RETURN n");
    assert!(bare.contains(r#""index":null"#), "{bare}");
}

#[test]
fn a_key_lookup_says_key() {
    let json = explain("MATCH (n:Person {id: 'p1'}) RETURN n");
    assert!(json.contains(r#""index":"key""#), "{json}");
}

#[test]
fn a_range_scan_names_its_index_and_bounds() {
    let json = explain("MATCH (n:Person) WHERE n.age > 30 RETURN n");
    assert!(json.contains(r#""op":"IndexRangeScan""#), "{json}");
    assert!(json.contains(r#""index":"idx_Person_age""#), "{json}");
    assert!(json.contains(r#""field":"age""#), "{json}");
    assert!(json.contains(r#""inclusive":false"#), "{json}");
}

#[test]
fn a_fused_scan_filter_is_marked() {
    let json = explain("MATCH (n:Person) WHERE n.age <> 30 RETURN n");
    assert!(json.contains(r#""op":"ScanLabel""#), "{json}");
    assert!(json.contains(r#""fused_filter":"age""#), "{json}");
}

#[test]
fn writes_show_their_read_part_and_write_ops() {
    let create = explain("CREATE (n:X {id: 'y'})");
    assert!(create.contains(r#""op":"Create""#), "{create}");
    assert!(create.contains(r#""label":"X""#), "{create}");

    let set = explain("MATCH (n:P) WHERE n.v > 1 SET n.w = 2");
    assert!(set.contains(r#""op":"IndexRangeScan""#), "{set}");
    assert!(set.contains(r#""op":"SetProperty""#), "{set}");

    let del = explain("MATCH (n:P) DETACH DELETE n");
    assert!(del.contains(r#""op":"ScanLabel""#), "{del}");
    assert!(del.contains(r#""op":"DeleteNode""#), "{del}");
    assert!(del.contains(r#""detach":true"#), "{del}");

    let merge = explain("MERGE (n:P {id: 'a'})");
    assert!(merge.contains(r#""op":"Merge""#), "{merge}");
    assert!(merge.contains(r#""index":"key""#), "{merge}");

    let unwind = explain("UNWIND [1,2] AS x CREATE (n:P {id: x})");
    assert!(unwind.contains(r#""op":"Unwind""#), "{unwind}");
    assert!(unwind.contains(r#""op":"Create""#), "{unwind}");

    let remove = explain("MATCH (n:P) REMOVE n.w");
    assert!(remove.contains(r#""op":"RemoveProperty""#), "{remove}");
}

#[test]
fn call_and_schema_statements_have_plans() {
    let call = explain("CALL db.ping() YIELD success RETURN success");
    assert!(call.contains(r#""op":"ProcedureCall""#), "{call}");
    assert!(call.contains(r#""procedure":"db.ping""#), "{call}");

    let ddl = explain("CREATE INDEX FOR (n:P) ON (n.age)");
    assert!(ddl.contains(r#""op":"Schema""#), "{ddl}");
    assert!(ddl.contains("CreateIndex"), "{ddl}");
}

#[test]
fn a_union_lists_each_part() {
    // The parts name one column: QL-VE refuses a UNION whose columns differ.
    let json = explain_bare("MATCH (a:A) RETURN a.x AS x UNION MATCH (b:B) RETURN b.x AS x");
    assert!(json.contains(r#""op":"Union""#), "{json}");
    assert!(
        json.contains(r#""label":"A""#) && json.contains(r#""label":"B""#),
        "{json}"
    );
}

#[test]
fn quotes_in_text_are_escaped() {
    let json = explain_bare("MATCH (n:P) WHERE n.name = 'a\"b' AND n.z <> 1 RETURN n");
    assert!(!json.contains("a\"b"), "{json}");
    assert!(json.contains("\\\""), "{json}");
}

#[test]
fn the_prefix_is_found_case_insensitively_and_profile_is_named() {
    let toks = lex("explain MATCH (n) RETURN n").unwrap();
    match explain_prefix(&toks) {
        ExplainPrefix::Explain(rest) => assert_eq!(rest, &toks[1..]),
        _ => panic!("explain prefix"),
    }
    assert!(matches!(
        explain_prefix(&lex("PROFILE MATCH (n) RETURN n").unwrap()),
        ExplainPrefix::Profile
    ));
    // The words stay ordinary names elsewhere.
    assert!(matches!(
        explain_prefix(&lex("MATCH (explain) RETURN explain").unwrap()),
        ExplainPrefix::None
    ));
    // A bare word with nothing after it is not a prefix.
    assert!(matches!(
        explain_prefix(&lex("explain").unwrap()),
        ExplainPrefix::None
    ));
}

#[test]
fn a_bad_statement_after_explain_is_a_parse_error() {
    let toks = lex("MATCH (n RETURN n").unwrap();
    assert!(explain_statement(&toks, &Declared).is_err());
}
