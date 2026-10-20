//! REMOVE, FOREACH, EXISTS, CALL and EXPLAIN were ordinary identifiers before
//! 0.10 and still are wherever a name can stand. Every query here parsed on
//! 0.9.x and must keep parsing.

use core_query::cypher::{lex, parse, Tok};

const WORDS: [&str; 5] = ["remove", "foreach", "exists", "call", "explain"];

fn casings(word: &str) -> [String; 3] {
    let mixed: String = word
        .chars()
        .enumerate()
        .map(|(i, c)| {
            if i % 2 == 0 {
                c.to_ascii_uppercase()
            } else {
                c
            }
        })
        .collect();
    [word.to_string(), word.to_ascii_uppercase(), mixed]
}

/// Query shapes with `§` standing for the word, each parseable on base.
const SHAPES: &[&str] = &[
    // variable, at end of input and before every kind of token
    "MATCH (§) RETURN §",
    "MATCH (§ {name: 'a'}) RETURN §",
    "MATCH (§:L) RETURN §.name",
    "MATCH (n) RETURN n AS §",
    "MATCH (n) WITH n AS § RETURN §",
    "MATCH (n) WITH n AS § WHERE §.x = 1 RETURN § ORDER BY §",
    "MATCH (n)-[§]->(m) RETURN §",
    "MATCH (n)-[§:T]->(m) RETURN §",
    "MATCH (n)-[:§]->(m) RETURN n",
    "MATCH (n)-[:A|:§]->(m) RETURN n",
    "MATCH (n:§) RETURN n",
    "MATCH (n:§ {§: 1, other: §}) RETURN n",
    "MATCH (n) RETURN n.§",
    "MATCH (n) WHERE n.§ = 1 RETURN n",
    "MATCH (n) WHERE n.§ IS NULL RETURN n",
    "MATCH (n) RETURN n.§ AS §",
    "MATCH (n) RETURN n ORDER BY n.§ DESC",
    "MATCH (n) RETURN §(n)",
    "MATCH (n) RETURN § (n)",
    "MATCH (n) WHERE §(n.x) RETURN n",
    "MATCH (n) WHERE §.x = 1 RETURN n",
    "MATCH (§) WHERE §.x[0] = 1 RETURN §",
    "UNWIND [1, 2] AS § RETURN §",
    "UNWIND [{§: 1}] AS m RETURN m.§",
    "MATCH (§) SET §.x = 1",
    "MATCH (§) DETACH DELETE §",
    "CREATE (§:T {id: 'a'}) RETURN §",
    "MERGE (§:T {id: 'a'}) RETURN §",
    // after every binary operator
    "MATCH (§) RETURN §.a * §.b",
    "MATCH (§) RETURN §.a / §.b",
    "MATCH (§) RETURN §.a + §.b",
    "MATCH (§) RETURN §.a - §.b",
    "MATCH (§) RETURN 1 * §",
    "MATCH (§) RETURN 1 / §",
    "MATCH (§) RETURN 1 + §",
    "MATCH (§) RETURN 1 - §",
    "MATCH (§) RETURN 1 % §",
    "MATCH (§) RETURN 1 ^ §",
    "MATCH (§) RETURN § % 2",
    "MATCH (§) RETURN § ^ 2",
    "MATCH (n) WHERE n.x = § RETURN n",
    "MATCH (n) WHERE n.x <> § RETURN n",
    "MATCH (n) WHERE n.x < § RETURN n",
    "MATCH (n) WHERE n.x <= § RETURN n",
    "MATCH (n) WHERE n.x > § RETURN n",
    "MATCH (n) WHERE n.x >= § RETURN n",
    "MATCH (§) WHERE §.x = 1 AND §.y = 2 RETURN §",
    "MATCH (§) WHERE §.x = 1 OR §.y = 2 RETURN §",
    "MATCH (§) WHERE NOT §.x = 1 RETURN §",
    "MATCH (§) WHERE §.x = 1 AND §.y = 2 OR §.z = 3 RETURN §",
    "MATCH (n) WHERE n.x IN § RETURN n",
    "MATCH (n) WHERE n.x IN [§.a, §.b] RETURN n",
    "MATCH (§) WHERE §.x IN [1, 2] RETURN §",
    "MATCH (n) WHERE n.x STARTS WITH §.y RETURN n",
    "MATCH (n) WHERE n.x CONTAINS §.y RETURN n",
    "MATCH (n) WHERE n.x ENDS WITH §.y RETURN n",
    "MATCH (§) WHERE §.x STARTS WITH 'a' RETURN §",
    "MATCH (§) WHERE §.x CONTAINS 'a' RETURN §",
    "MATCH (§) WHERE §.x IS NOT NULL RETURN §",
    "MATCH (n) RETURN CASE WHEN n.a THEN §.x ELSE §.y END",
    // list and map positions
    "MATCH (§) RETURN [§.a, §.b]",
    "MATCH (§) RETURN §.a[0]",
    "MATCH (n) RETURN count(§)",
    "MATCH (§) RETURN count(§) AS W2",
    // after a clause keyword
    "MATCH (n) WITH § RETURN 1",
    "MATCH (n) RETURN §",
    "MATCH (n) RETURN DISTINCT §",
    "MATCH (n), (§) RETURN n, §",
    "MATCH (n) WITH n MATCH (§) RETURN §",
    "MATCH (n) WITH n OPTIONAL MATCH (n)-[§]->(m) RETURN §",
    "MATCH (n) RETURN n ORDER BY §",
    "MATCH (n) RETURN n SKIP 1 LIMIT 2",
];

fn parses(src: &str) -> bool {
    match lex(src) {
        Ok(toks) => {
            parse(&toks).is_ok()
                || core_query::cypher::parse_read(&toks).is_ok()
                || core_query::cypher::parse_write(&toks).is_ok()
        }
        Err(_) => false,
    }
}

/// A shape is a regression if it parses with an ordinary name (`zq`, standing
/// for base behaviour, where the five words were ordinary names too) and does
/// not parse with one of the five words.
#[test]
fn the_five_words_are_names_in_every_position() {
    let mut checked = 0;
    for shape in SHAPES {
        if !parses(&shape.replace('§', "zq")) {
            eprintln!("skipped (unsupported on base): {shape}");
            continue;
        }
        for word in WORDS {
            for w in casings(word) {
                let src = shape.replace('§', &w);
                assert!(parses(&src), "does not parse: {src}");
                checked += 1;
            }
        }
    }
    assert!(checked >= 15 * 60, "too few shapes exercised: {checked}");
}

#[test]
fn the_five_words_lex_as_identifiers() {
    for word in WORDS {
        for w in casings(word) {
            assert_eq!(lex(&w).unwrap(), vec![Tok::Ident(w.clone())]);
        }
    }
}
