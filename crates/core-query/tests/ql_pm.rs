//! QL-PM: a `CREATE` or `MERGE` whose property map reads a `$parameter`, or
//! that takes a whole map (`CREATE (n:P $props)`, `MERGE … ON CREATE SET n +=
//! $props`), parses to the per-row form, which evaluates the values against
//! the statement's parameters. A literal-only statement keeps its own form.

use core_query::cypher::{
    lex, parse_write, MergeSet, Operand, RowWrite, UnwindWriteStmt, WriteStatement,
};

fn parse(q: &str) -> Result<WriteStatement, String> {
    parse_write(&lex(q).unwrap())
}

fn rows(q: &str) -> UnwindWriteStmt {
    match parse(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        WriteStatement::UnwindWrite(s) => *s,
        other => panic!("{q}: expected the per-row form, got {other:?}"),
    }
}

#[test]
fn a_literal_create_or_merge_keeps_its_own_form() {
    assert!(matches!(
        parse("CREATE (:P {id: 'a', v: 1 + 1})").unwrap(),
        WriteStatement::Create(_)
    ));
    assert!(matches!(
        parse("MERGE (n:P {id: 'a'}) ON CREATE SET n.v = $v").unwrap(),
        WriteStatement::Merge(_)
    ));
}

#[test]
fn a_parameter_in_a_create_property_map_is_the_per_row_form() {
    for q in [
        "CREATE (:P {id: $id})",
        "CREATE (:P {id: 'a', v: $v})",
        "CREATE (:P {id: $p.id, l: [1, $x], m: {k: $x}, n: $x + 1, u: toUpper($s)})",
        "CREATE (a:P {id: 'a'})-[:T]->(b:Q {id: $b}) RETURN a, b",
    ] {
        let s = rows(q);
        assert!(
            matches!(s.writes.as_slice(), [RowWrite::Create { .. }]),
            "{q}"
        );
    }
}

#[test]
fn a_parameter_in_a_merge_key_is_the_per_row_form() {
    for q in [
        "MERGE (n:P {id: $id})",
        "MERGE (n:P {id: $id}) ON CREATE SET n.v = $v RETURN n",
        "MERGE (n:P {id: 'a', ns: $ns})",
        "MERGE (a:P {id: 'a'})-[:K]->(b:Q {id: $b})",
    ] {
        let s = rows(q);
        assert!(matches!(s.writes.as_slice(), [RowWrite::Merge(_)]), "{q}");
    }
}

#[test]
fn create_takes_a_whole_map_parameter() {
    let s = rows("CREATE (n:P $props) RETURN n");
    match s.writes.as_slice() {
        [RowWrite::Create { nodes, .. }] => {
            assert!(nodes[0].props.is_empty());
            assert_eq!(nodes[0].props_map, Some(Operand::Param("props".into())));
        }
        other => panic!("{other:?}"),
    }
    // After UNWIND too.
    let s = rows("UNWIND $rows AS r CREATE (n:P $props)");
    assert!(matches!(s.writes.as_slice(), [RowWrite::Create { .. }]));
}

#[test]
fn merge_set_takes_a_map() {
    let s =
        rows("MERGE (n:P {id: $id}) ON CREATE SET n += $props ON MATCH SET n = {a: 1}, n.b = 2");
    let [RowWrite::Merge(m)] = s.writes.as_slice() else {
        panic!("{:?}", s.writes)
    };
    assert!(matches!(
        m.on_create.as_slice(),
        [MergeSet::Map { merge: true, value: Operand::Param(p), .. }] if p == "props"
    ));
    assert!(matches!(
        m.on_match.as_slice(),
        [MergeSet::Map { merge: false, .. }, MergeSet::Prop(sc)] if sc.field == "b"
    ));
    // A literal MERGE with a map SET is the per-row form as well.
    let s = rows("MERGE (n:P {id: 'a'}) ON CREATE SET n += {v: 1}");
    assert!(matches!(s.writes.as_slice(), [RowWrite::Merge(_)]));
    // A map SET on a name that is not a MERGE node is refused.
    let e = parse("MERGE (n:P {id: $id}) ON CREATE SET m += $props").unwrap_err();
    assert!(
        e.contains("does not match a MERGE node or relationship variable"),
        "{e}"
    );
}

#[test]
fn a_parameter_map_in_a_merge_pattern_is_refused_by_name() {
    for q in [
        "MERGE (n:P $props)",
        "MERGE (a:P {id: 'a'})-[:K]->(b:Q $props)",
    ] {
        let e = parse(q).unwrap_err();
        assert!(
            e.starts_with("not supported in mushroomdb 0.10"),
            "{q}: {e}"
        );
        assert!(e.contains("$parameter map in a MERGE pattern"), "{q}: {e}");
    }
}

#[test]
fn an_unbound_variable_in_a_plain_create_keeps_a_named_error() {
    let e = parse("CREATE (:P {id: x})").unwrap_err();
    assert!(!e.contains("literals only"), "{e}");
    assert!(e.contains("variable"), "{e}");
}
