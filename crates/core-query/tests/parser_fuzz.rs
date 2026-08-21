//! Parser robustness: `lex` + `parse` return `Ok` or `Err` and never panic.
//!
//! Three generators, 256 cases each (768 total):
//!   (a) arbitrary byte strings, UTF-8 lossy — **lexer-robustness**. Most
//!       inputs are illegal at lex time, so `lex_then_parse` never reaches
//!       `parse`. That is intentional for this block.
//!   (b) token-level shuffle / truncate / duplicate of a pool of valid queries
//!       that together cover every grammar production
//!   (c) arbitrary `Vec<Tok>` (every variant, length 0..40) fed straight to
//!       `parse` — this is the parser-on-hostile-token-streams block.

use core_query::cypher::{lex, parse, Tok};
use proptest::prelude::*;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// At least 10 valid queries. Comments name the productions each one pins.
/// Together they cover the binding grammar:
///   query, match+, pattern, node (var / label / props / all optional),
///   props (single + multi, every operand kind), rel (Right / Left /
///   Undirected, var/etype optional), WHERE (OR / AND / NOT / paren / all
///   six cmp ops), operands (prop / lit / param), RETURN (var / prop / AS /
///   multi), ORDER BY (alias / var / prop, ASC / DESC), SKIP, LIMIT.
const VALID: &[&str] = &[
    // node: var only; RETURN var; minimal query
    "MATCH (a) RETURN a",
    // node: anonymous + unlabeled; rel: typed, no var, Right; dest var
    "MATCH ()-[:E]->(b) RETURN b",
    // node: var + props (param + int + str); RETURN prop AS alias
    "MATCH (n {k: $key, n: 1, s: 'hi'}) RETURN n.k AS x",
    // node: var + label; rel: var + type, Right; dest label; multi RETURN
    "MATCH (a:L0)-[r:E]->(b:L1) RETURN a, b",
    // rel: Left typed no var; Undirected anonymous; anonymous mid-node
    "MATCH (a)<-[:E]-()-[]-(c) RETURN a, c",
    // query: match_clause+ (two MATCH); rel var; RETURN rel prop
    "MATCH (a:L0) MATCH (b:L1)-[r:E]->(a) RETURN a, r.score",
    // WHERE: NOT, paren, OR, AND; cmp = <> < <= > >=; lit int + float
    "MATCH (a) WHERE NOT (a.x = 1 OR a.y <> 2) AND a.z < 3 AND a.p <= 4 AND a.q > 5 AND a.r >= 6.5 RETURN a",
    // operand: string lit with escape; param; AND
    "MATCH (a) WHERE a.name = 'it\\'s' AND a.id = $id RETURN a",
    // ORDER BY alias ASC, bare var, var.ident DESC; RETURN prop AS
    "MATCH (a)-[r:E]->(b) RETURN a, b.name AS n ORDER BY n ASC, a, b.name DESC",
    // SKIP + LIMIT
    "MATCH (a) RETURN a SKIP 2 LIMIT 3",
    // unary-minus props; rel Right no type; rel Left typed; chain
    "MATCH (a {x: -5, y: -1.5})-[r]->(b)<-[s:T]-(c) RETURN a",
    // kitchen sink: every remaining combo (multi MATCH-adjacent chain,
    // props mixed operands, WHERE NOT/AND/OR, RETURN var/prop/AS,
    // ORDER BY alias DESC + prop ASC, SKIP, LIMIT)
    "MATCH (a:Person {name: $n, age: 30})-[r:KNOWS]->(b)-[u:TEAM]-(c)<-[s:LIKES]-(d) \
     WHERE NOT a.age < 18 AND b.name = 'x' OR c.score >= 2.5 \
     RETURN a, r.since AS since, b.name \
     ORDER BY since DESC, b.name ASC \
     SKIP 1 LIMIT 5",
];

fn render_tok(t: &Tok) -> String {
    match t {
        Tok::Match => "MATCH".into(),
        Tok::Where => "WHERE".into(),
        Tok::Return => "RETURN".into(),
        Tok::Order => "ORDER".into(),
        Tok::By => "BY".into(),
        Tok::Skip => "SKIP".into(),
        Tok::Limit => "LIMIT".into(),
        Tok::As => "AS".into(),
        Tok::And => "AND".into(),
        Tok::Or => "OR".into(),
        Tok::Not => "NOT".into(),
        Tok::Asc => "ASC".into(),
        Tok::Desc => "DESC".into(),
        Tok::Ident(s) => s.clone(),
        Tok::Str(s) => format!("'{}'", s.replace('\'', "\\'")),
        Tok::Int(n) => n.to_string(),
        Tok::Float(f) => f.to_string(),
        Tok::Param(s) => format!("${s}"),
        Tok::LParen => "(".into(),
        Tok::RParen => ")".into(),
        Tok::LBracket => "[".into(),
        Tok::RBracket => "]".into(),
        Tok::LBrace => "{".into(),
        Tok::RBrace => "}".into(),
        Tok::Colon => ":".into(),
        Tok::Comma => ",".into(),
        Tok::Dot => ".".into(),
        Tok::Eq => "=".into(),
        Tok::Ne => "<>".into(),
        Tok::Lt => "<".into(),
        Tok::Le => "<=".into(),
        Tok::Gt => ">".into(),
        Tok::Ge => ">=".into(),
        Tok::Dash => "-".into(),
        Tok::Star => "*".into(),
        Tok::Create => "CREATE".into(),
        Tok::Set => "SET".into(),
        Tok::Delete => "DELETE".into(),
        Tok::Detach => "DETACH".into(),
        Tok::Merge => "MERGE".into(),
        Tok::With => "WITH".into(),
        Tok::Unwind => "UNWIND".into(),
    }
}

/// Token substrings of a valid query (lex must succeed on `VALID` entries).
fn token_substrings(src: &str) -> Vec<String> {
    lex(src)
        .expect("VALID pool entry must lex")
        .iter()
        .map(render_tok)
        .collect()
}

/// Shuffle, truncate, and/or duplicate token substrings using only
/// proptest-provided entropy (no `rand`, no thread RNG).
fn mutate_tokens(mut toks: Vec<String>, mode: u8, entropy: &[u8]) -> String {
    if toks.is_empty() {
        return String::new();
    }
    match mode % 4 {
        0 => shuffle_tokens(&mut toks, entropy),
        1 => truncate_tokens(&mut toks, entropy),
        2 => duplicate_token(&mut toks, entropy),
        _ => {
            duplicate_token(&mut toks, entropy);
            truncate_tokens(&mut toks, entropy.get(2..).unwrap_or(entropy));
            shuffle_tokens(&mut toks, entropy);
        }
    }
    toks.join(" ")
}

fn shuffle_tokens(toks: &mut [String], entropy: &[u8]) {
    if toks.len() < 2 {
        return;
    }
    for (i, &e) in entropy.iter().enumerate() {
        let j = (e as usize) % toks.len();
        toks.swap(i % toks.len(), j);
    }
}

fn truncate_tokens(toks: &mut Vec<String>, entropy: &[u8]) {
    let keep = match entropy.first() {
        Some(&e) => (e as usize) % (toks.len() + 1),
        None => 0,
    };
    toks.truncate(keep);
}

fn duplicate_token(toks: &mut Vec<String>, entropy: &[u8]) {
    let i = match entropy.first() {
        Some(&e) => (e as usize) % toks.len(),
        None => 0,
    };
    let token = toks[i].clone();
    let j = match entropy.get(1) {
        Some(&e) => (e as usize) % (toks.len() + 1),
        None => toks.len(),
    };
    toks.insert(j, token);
}

fn lex_then_parse(src: &str) {
    if let Ok(toks) = lex(src) {
        let _ = parse(&toks);
    }
}

fn never_panics(src: &str) -> Result<(), TestCaseError> {
    let outcome = catch_unwind(AssertUnwindSafe(|| lex_then_parse(src)));
    prop_assert!(outcome.is_ok(), "lex+parse panicked on input {:?}", src);
    Ok(())
}

fn parse_never_panics(toks: &[Tok]) -> Result<(), TestCaseError> {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let _ = parse(toks);
    }));
    prop_assert!(outcome.is_ok(), "parse panicked on tokens {:?}", toks);
    Ok(())
}

fn ident_like() -> impl Strategy<Value = String> {
    proptest::collection::vec(prop::char::range('a', 'z'), 0..8)
        .prop_map(|cs| cs.into_iter().collect())
}

fn str_like() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<u8>(), 0..16)
        .prop_map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Every unit `Tok` variant, uniformly.
fn unit_tok() -> impl Strategy<Value = Tok> {
    (0u8..29).prop_map(|i| match i {
        0 => Tok::Match,
        1 => Tok::Where,
        2 => Tok::Return,
        3 => Tok::Order,
        4 => Tok::By,
        5 => Tok::Skip,
        6 => Tok::Limit,
        7 => Tok::As,
        8 => Tok::And,
        9 => Tok::Or,
        10 => Tok::Not,
        11 => Tok::Asc,
        12 => Tok::Desc,
        13 => Tok::LParen,
        14 => Tok::RParen,
        15 => Tok::LBracket,
        16 => Tok::RBracket,
        17 => Tok::LBrace,
        18 => Tok::RBrace,
        19 => Tok::Colon,
        20 => Tok::Comma,
        21 => Tok::Dot,
        22 => Tok::Eq,
        23 => Tok::Ne,
        24 => Tok::Lt,
        25 => Tok::Le,
        26 => Tok::Gt,
        27 => Tok::Ge,
        _ => Tok::Dash,
    })
}

/// Sample from all 34 `Tok` variants, including random Ident/Str/Int/Float/Param.
fn tok_strategy() -> impl Strategy<Value = Tok> {
    prop_oneof![
        29 => unit_tok(),
        1 => ident_like().prop_map(Tok::Ident),
        1 => str_like().prop_map(Tok::Str),
        1 => any::<i64>().prop_map(Tok::Int),
        1 => any::<f64>().prop_map(Tok::Float),
        1 => ident_like().prop_map(Tok::Param),
    ]
}

#[test]
fn valid_pool_has_at_least_ten_and_each_parses() {
    assert!(
        VALID.len() >= 10,
        "pool must have at least 10 valid queries, got {}",
        VALID.len()
    );
    for src in VALID {
        let toks = lex(src).unwrap_or_else(|e| panic!("VALID query must lex: {src:?}: {e}"));
        parse(&toks).unwrap_or_else(|e| panic!("VALID query must parse: {src:?}: {e}"));
    }
}

// Block (a): lexer-robustness. Arbitrary bytes are almost always rejected
// by `lex`, so this block rarely reaches `parse`. Parser coverage is (b)+(c).
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn lex_parse_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let src = String::from_utf8_lossy(&bytes).into_owned();
        never_panics(&src)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn lex_parse_never_panics_on_mutated_valid_queries(
        qi in 0..VALID.len(),
        mode in any::<u8>(),
        entropy in proptest::collection::vec(any::<u8>(), 0..48)
    ) {
        let src = mutate_tokens(token_substrings(VALID[qi]), mode, &entropy);
        never_panics(&src)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn parse_never_panics_on_arbitrary_token_sequences(
        toks in proptest::collection::vec(tok_strategy(), 0..40)
    ) {
        parse_never_panics(&toks)?;
    }
}
