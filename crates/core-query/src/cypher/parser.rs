//! Recursive-descent parser for the Cypher subset. Never panics on any token sequence.

use super::ast::{
    AggArg, AggFunc, Expr, HopRange, NodePat, Operand, OrderItem, OrderTarget, Pattern, Query,
    RelDir, RelPat, RetItem, RetVal,
};
use super::Tok;
use crate::filter::CmpOp;
use core_storage::Value;

/// Max parenthesized-expression nesting. Deeper input is `Err`, not a stack overflow.
const MAX_PAREN_DEPTH: usize = 64;

/// Parse a tokenized Cypher subset query. Every failure is `Err(String)`; this
/// function never panics on a well-formed `&[Tok]` (including empty / garbage).
pub fn parse(tokens: &[Tok]) -> Result<Query, String> {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
    };
    p.query()
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.toks.get(self.pos)
    }

    fn eat(&mut self, want: &Tok) -> bool {
        if self.peek() == Some(want) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn err(&self, msg: &str) -> String {
        match self.peek() {
            Some(tok) => format!("parse error at token {}: {msg} (found {tok:?})", self.pos),
            None => format!(
                "parse error at token {}: {msg} (found end of input)",
                self.pos
            ),
        }
    }

    fn expect(&mut self, want: &Tok, what: &str) -> Result<(), String> {
        if self.eat(want) {
            Ok(())
        } else {
            Err(self.err(what))
        }
    }

    fn ident(&mut self, what: &str) -> Result<String, String> {
        match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            _ => Err(self.err(what)),
        }
    }

    fn query(&mut self) -> Result<Query, String> {
        let mut matches = Vec::new();
        while self.peek() == Some(&Tok::Match) {
            matches.push(self.match_clause()?);
        }
        if matches.is_empty() {
            return Err(self.err("expected MATCH"));
        }
        let where_expr = if self.eat(&Tok::Where) {
            Some(self.expr(0)?)
        } else {
            None
        };
        let returns = self.return_clause()?;
        let aliases: Vec<&str> = returns.iter().filter_map(|r| r.alias.as_deref()).collect();
        let order_by = if self.peek() == Some(&Tok::Order) {
            self.order_clause(&aliases)?
        } else {
            Vec::new()
        };
        let skip = if self.eat(&Tok::Skip) {
            Some(self.uint("SKIP")?)
        } else {
            None
        };
        let limit = if self.eat(&Tok::Limit) {
            Some(self.uint("LIMIT")?)
        } else {
            None
        };
        if self.pos < self.toks.len() {
            return Err(self.err("unexpected tokens after query"));
        }
        Ok(Query {
            matches,
            where_expr,
            returns,
            order_by,
            skip,
            limit,
        })
    }

    fn match_clause(&mut self) -> Result<Pattern, String> {
        self.expect(&Tok::Match, "expected MATCH")?;
        // Detect `MATCH shortestPath(...)`.
        if let Some(Tok::Ident(s)) = self.peek() {
            if s.eq_ignore_ascii_case("shortestpath") {
                return self.shortest_path_clause();
            }
        }
        self.pattern()
    }

    fn shortest_path_clause(&mut self) -> Result<Pattern, String> {
        self.pos += 1; // consume "shortestPath" identifier
        self.expect(
            &Tok::LParen,
            "expected '(' after shortestPath",
        )?;
        let start = self.node()?;
        let rel = self.rel()?;
        if rel.hops.is_none() {
            return Err(self.err(
                "shortestPath requires a variable-length relationship (e.g. [*..5])",
            ));
        }
        let dest = self.node()?;
        self.expect(
            &Tok::RParen,
            "expected ')' to close shortestPath",
        )?;
        Ok(Pattern {
            start,
            chain: vec![(rel, dest)],
            shortest: true,
        })
    }

    fn pattern(&mut self) -> Result<Pattern, String> {
        let start = self.node()?;
        let mut chain = Vec::new();
        while matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
            let rel = self.rel()?;
            let dest = self.node()?;
            chain.push((rel, dest));
        }
        Ok(Pattern {
            start,
            chain,
            shortest: false,
        })
    }

    fn node(&mut self) -> Result<NodePat, String> {
        self.expect(&Tok::LParen, "expected '(' to start a node pattern")?;
        let var = match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.pos += 1;
                Some(s)
            }
            _ => None,
        };
        let label = if self.eat(&Tok::Colon) {
            Some(self.ident("expected label identifier after ':'")?)
        } else {
            None
        };
        let props = if self.peek() == Some(&Tok::LBrace) {
            self.props()?
        } else {
            Vec::new()
        };
        self.expect(&Tok::RParen, "expected ')' to close a node pattern")?;
        Ok(NodePat { var, label, props })
    }

    fn props(&mut self) -> Result<Vec<(String, Operand)>, String> {
        self.expect(&Tok::LBrace, "expected '{'")?;
        let mut out = Vec::new();
        loop {
            let key = self.ident("expected property key")?;
            self.expect(&Tok::Colon, "expected ':' after property key")?;
            let val = self.operand()?;
            out.push((key, val));
            if self.eat(&Tok::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Tok::RBrace, "expected '}' to close property map")?;
        Ok(out)
    }

    fn rel(&mut self) -> Result<RelPat, String> {
        if self.eat(&Tok::Lt) {
            self.expect(
                &Tok::Dash,
                "expected '-' after '<' in a left-directed relationship",
            )?;
            let (var, etype, hops) = self.rel_body()?;
            self.expect(
                &Tok::Dash,
                "expected '-' to close a left-directed relationship",
            )?;
            return Ok(RelPat {
                var,
                etype,
                dir: RelDir::Left,
                hops,
            });
        }
        self.expect(&Tok::Dash, "expected '-' to start a relationship")?;
        let (var, etype, hops) = self.rel_body()?;
        self.expect(&Tok::Dash, "expected '-' after ']'")?;
        let dir = if self.eat(&Tok::Gt) {
            RelDir::Right
        } else {
            RelDir::Undirected
        };
        Ok(RelPat {
            var,
            etype,
            dir,
            hops,
        })
    }

    #[allow(clippy::type_complexity)]
    fn rel_body(&mut self) -> Result<(Option<String>, Option<String>, Option<HopRange>), String> {
        self.expect(&Tok::LBracket, "expected '[' in a relationship pattern")?;
        let var = match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.pos += 1;
                Some(s)
            }
            _ => None,
        };
        let etype = if self.eat(&Tok::Colon) {
            Some(self.ident("expected relationship type identifier after ':'")?)
        } else {
            None
        };
        let hops = if self.eat(&Tok::Star) {
            Some(self.parse_hop_range()?)
        } else {
            None
        };
        self.expect(
            &Tok::RBracket,
            "expected ']' to close a relationship pattern",
        )?;
        Ok((var, etype, hops))
    }

    /// Parse the hop-count range that follows `*` inside a relationship bracket.
    ///
    /// Recognised forms (after `*` is already consumed):
    /// - `]`        → bare `*`, treated as `1..10`
    /// - `n`        → exactly n hops (`n..n`)
    /// - `n..m`     → n..m hops
    /// - `..m`      → 1..m hops
    /// - `n..`      → unbounded → hard-cap error
    ///
    /// Hard cap: max > 10 or unbounded → Err("variable-length paths are capped at 10 hops").
    fn parse_hop_range(&mut self) -> Result<HopRange, String> {
        const CAP_ERR: &str = "variable-length paths are capped at 10 hops";

        match self.peek() {
            // bare `*`  →  min=1, max=10
            Some(Tok::RBracket) => Ok(HopRange { min: 1, max: 10 }),

            // `*n`  or  `*n..`  or  `*n..m`
            Some(Tok::Int(n)) => {
                let n = *n;
                self.pos += 1;
                if self.eat(&Tok::Dot) {
                    self.expect(&Tok::Dot, "expected '..' separator in hop range")?;
                    match self.peek() {
                        Some(Tok::Int(m)) => {
                            let m = *m;
                            self.pos += 1;
                            // `*n..m`
                            self.validate_hop_range(n, m, CAP_ERR)
                        }
                        _ => {
                            // `*n..` — unbounded
                            Err(CAP_ERR.to_string())
                        }
                    }
                } else {
                    // `*n` — exact hops
                    self.validate_hop_range(n, n, CAP_ERR)
                }
            }

            // `*..m`
            Some(Tok::Dot) => {
                self.pos += 1; // consume first '.'
                self.expect(&Tok::Dot, "expected '..' range separator after '*'")?;
                match self.peek() {
                    Some(Tok::Int(m)) => {
                        let m = *m;
                        self.pos += 1;
                        self.validate_hop_range(1, m, CAP_ERR)
                    }
                    _ => Err(self.err("expected max-hop integer after '*..'"))
                }
            }

            // Anything else after `*` — treat as bare `*`
            _ => Ok(HopRange { min: 1, max: 10 }),
        }
    }

    fn validate_hop_range(&self, min_n: i64, max_n: i64, cap_err: &str) -> Result<HopRange, String> {
        if min_n < 0 || max_n < 0 {
            return Err(self.err("hop counts must be non-negative"));
        }
        if max_n > 10 {
            return Err(cap_err.to_string());
        }
        let min = min_n as u8;
        let max = max_n as u8;
        if min > max {
            return Err(self.err(&format!(
                "variable-length path min ({min}) must not exceed max ({max})"
            )));
        }
        Ok(HopRange { min, max })
    }

    fn expr(&mut self, paren_depth: usize) -> Result<Expr, String> {
        let mut left = self.term(paren_depth)?;
        while self.eat(&Tok::Or) {
            let right = self.term(paren_depth)?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn term(&mut self, paren_depth: usize) -> Result<Expr, String> {
        let mut left = self.factor(paren_depth)?;
        while self.eat(&Tok::And) {
            let right = self.factor(paren_depth)?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn factor(&mut self, paren_depth: usize) -> Result<Expr, String> {
        let negated = self.eat(&Tok::Not);
        let inner = if self.eat(&Tok::LParen) {
            if paren_depth >= MAX_PAREN_DEPTH {
                return Err(self.err("expression nesting too deep"));
            }
            let e = self.expr(paren_depth + 1)?;
            self.expect(
                &Tok::RParen,
                "expected ')' to close parenthesized expression",
            )?;
            e
        } else {
            self.cmp()?
        };
        if negated {
            Ok(Expr::Not(Box::new(inner)))
        } else {
            Ok(inner)
        }
    }

    fn cmp(&mut self) -> Result<Expr, String> {
        let lhs = self.operand()?;
        let op = self.cmp_op()?;
        let rhs = self.operand()?;
        Ok(Expr::Cmp { lhs, op, rhs })
    }

    fn cmp_op(&mut self) -> Result<CmpOp, String> {
        let op = match self.peek() {
            Some(Tok::Eq) => CmpOp::Eq,
            Some(Tok::Ne) => CmpOp::Ne,
            Some(Tok::Lt) => CmpOp::Lt,
            Some(Tok::Le) => CmpOp::Le,
            Some(Tok::Gt) => CmpOp::Gt,
            Some(Tok::Ge) => CmpOp::Ge,
            _ => return Err(self.err("expected comparison operator")),
        };
        self.pos += 1;
        Ok(op)
    }

    fn operand(&mut self) -> Result<Operand, String> {
        if self.eat(&Tok::Dash) {
            return match self.peek() {
                Some(Tok::Int(n)) => {
                    let n = *n;
                    self.pos += 1;
                    let neg = n
                        .checked_neg()
                        .ok_or_else(|| self.err("integer negation overflow"))?;
                    Ok(Operand::Lit(Value::Int(neg)))
                }
                Some(Tok::Float(x)) => {
                    let x = *x;
                    self.pos += 1;
                    Ok(Operand::Lit(Value::Float(-x)))
                }
                _ => Err(self.err("unary minus only applies to numeric literals")),
            };
        }
        match self.peek() {
            Some(Tok::Int(n)) => {
                let n = *n;
                self.pos += 1;
                Ok(Operand::Lit(Value::Int(n)))
            }
            Some(Tok::Float(x)) => {
                let x = *x;
                self.pos += 1;
                Ok(Operand::Lit(Value::Float(x)))
            }
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(Operand::Lit(Value::Str(s)))
            }
            Some(Tok::Param(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(Operand::Param(s))
            }
            Some(Tok::Ident(_)) => {
                let var = self.ident("expected identifier")?;
                self.expect(&Tok::Dot, "expected '.' after variable in operand")?;
                let field = self.ident("expected field name after '.'")?;
                Ok(Operand::Prop { var, field })
            }
            _ => Err(self.err("expected operand (property, literal, or parameter)")),
        }
    }

    fn return_clause(&mut self) -> Result<Vec<RetItem>, String> {
        self.expect(&Tok::Return, "expected RETURN")?;
        let mut items = vec![self.ret_item()?];
        while self.eat(&Tok::Comma) {
            items.push(self.ret_item()?);
        }
        Ok(items)
    }

    fn ret_item(&mut self) -> Result<RetItem, String> {
        // Check for an aggregate function name (COUNT/SUM/AVG/MIN/MAX).
        // These are ordinary identifiers in the lexer, so we peek and check
        // the lowercased string before deciding the parse branch.
        if let Some(Tok::Ident(s)) = self.peek() {
            let func = match s.to_ascii_lowercase().as_str() {
                "count" => Some(AggFunc::Count),
                "sum" => Some(AggFunc::Sum),
                "avg" => Some(AggFunc::Avg),
                "min" => Some(AggFunc::Min),
                "max" => Some(AggFunc::Max),
                _ => None,
            };
            if let Some(func) = func {
                self.pos += 1; // consume the function name
                self.expect(&Tok::LParen, "expected '(' after aggregate function name")?;
                let arg = if self.eat(&Tok::Star) {
                    AggArg::Star
                } else {
                    let var = self.ident("expected variable or '*' in aggregate argument")?;
                    if self.eat(&Tok::Dot) {
                        let field =
                            self.ident("expected field name after '.' in aggregate argument")?;
                        AggArg::Prop { var, field }
                    } else {
                        AggArg::Var(var)
                    }
                };
                self.expect(&Tok::RParen, "expected ')' to close aggregate function")?;
                let alias = if self.eat(&Tok::As) {
                    Some(self.ident("expected alias identifier after AS")?)
                } else {
                    None
                };
                return Ok(RetItem {
                    value: RetVal::Agg { func, arg },
                    alias,
                });
            }
        }

        // Normal (non-aggregate) RETURN item.
        let var = self.ident("expected variable in RETURN item")?;
        let value = if self.eat(&Tok::Dot) {
            let field = self.ident("expected field name after '.'")?;
            RetVal::Prop { var, field }
        } else {
            RetVal::Var(var)
        };
        let alias = if self.eat(&Tok::As) {
            Some(self.ident("expected alias identifier after AS")?)
        } else {
            None
        };
        Ok(RetItem { value, alias })
    }

    fn order_clause(&mut self, aliases: &[&str]) -> Result<Vec<OrderItem>, String> {
        self.expect(&Tok::Order, "expected ORDER")?;
        self.expect(&Tok::By, "expected BY after ORDER")?;
        let mut items = vec![self.order_item(aliases)?];
        while self.eat(&Tok::Comma) {
            items.push(self.order_item(aliases)?);
        }
        Ok(items)
    }

    fn order_item(&mut self, aliases: &[&str]) -> Result<OrderItem, String> {
        let name = self.ident("expected ORDER BY target")?;
        let target = if self.eat(&Tok::Dot) {
            let field = self.ident("expected field name after '.'")?;
            OrderTarget::Prop { var: name, field }
        } else if aliases.contains(&name.as_str()) {
            OrderTarget::Alias(name)
        } else {
            OrderTarget::Var(name)
        };
        let descending = if self.eat(&Tok::Desc) {
            true
        } else {
            let _ = self.eat(&Tok::Asc);
            false
        };
        Ok(OrderItem { target, descending })
    }

    fn uint(&mut self, what: &str) -> Result<u64, String> {
        match self.peek() {
            Some(Tok::Int(n)) if *n >= 0 => {
                let n = *n as u64;
                self.pos += 1;
                Ok(n)
            }
            Some(Tok::Int(_)) => Err(self.err(&format!("{what} must be a non-negative integer"))),
            _ => Err(self.err(&format!("expected integer after {what}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::cypher::ast::{
        Expr, HopRange, NodePat, Operand, OrderItem, OrderTarget, Pattern, Query, RelDir, RelPat,
        RetItem, RetVal,
    };
    use crate::cypher::{lex, Tok};
    use crate::filter::CmpOp;
    use core_storage::Value;

    fn parse_src(src: &str) -> Result<Query, String> {
        parse(&lex(src)?)
    }

    fn prop(var: &str, field: &str) -> Operand {
        Operand::Prop {
            var: var.into(),
            field: field.into(),
        }
    }

    fn cmp(lhs: Operand, op: CmpOp, rhs: Operand) -> Expr {
        Expr::Cmp { lhs, op, rhs }
    }

    fn node(var: Option<&str>, label: Option<&str>, props: Vec<(String, Operand)>) -> NodePat {
        NodePat {
            var: var.map(str::to_string),
            label: label.map(str::to_string),
            props,
        }
    }

    #[test]
    fn full_feature_query_exact_ast() {
        let src = "\
MATCH (a:Person {name: $n, age: 30})-[r:KNOWS]->(b)-[u:TEAM]-(c)<-[s:LIKES]-(d) \
WHERE NOT a.age < 18 AND b.name = 'x' OR c.score >= 2.5 \
RETURN a, r.since AS since, b.name \
ORDER BY since DESC, b.name ASC \
SKIP 1 LIMIT 5";
        let got = parse_src(src).expect("full-feature query must parse");
        let expected = Query {
            matches: vec![Pattern {
                start: node(
                    Some("a"),
                    Some("Person"),
                    vec![
                        ("name".into(), Operand::Param("n".into())),
                        ("age".into(), Operand::Lit(Value::Int(30))),
                    ],
                ),
                chain: vec![
                    (
                        RelPat {
                            var: Some("r".into()),
                            etype: Some("KNOWS".into()),
                            dir: RelDir::Right,
                            hops: None,
                        },
                        node(Some("b"), None, vec![]),
                    ),
                    (
                        RelPat {
                            var: Some("u".into()),
                            etype: Some("TEAM".into()),
                            dir: RelDir::Undirected,
                            hops: None,
                        },
                        node(Some("c"), None, vec![]),
                    ),
                    (
                        RelPat {
                            var: Some("s".into()),
                            etype: Some("LIKES".into()),
                            dir: RelDir::Left,
                            hops: None,
                        },
                        node(Some("d"), None, vec![]),
                    ),
                ],
                shortest: false,
            }],
            where_expr: Some(Expr::Or(
                Box::new(Expr::And(
                    Box::new(Expr::Not(Box::new(cmp(
                        prop("a", "age"),
                        CmpOp::Lt,
                        Operand::Lit(Value::Int(18)),
                    )))),
                    Box::new(cmp(
                        prop("b", "name"),
                        CmpOp::Eq,
                        Operand::Lit(Value::Str("x".into())),
                    )),
                )),
                Box::new(cmp(
                    prop("c", "score"),
                    CmpOp::Ge,
                    Operand::Lit(Value::Float(2.5)),
                )),
            )),
            returns: vec![
                RetItem {
                    value: RetVal::Var("a".into()),
                    alias: None,
                },
                RetItem {
                    value: RetVal::Prop {
                        var: "r".into(),
                        field: "since".into(),
                    },
                    alias: Some("since".into()),
                },
                RetItem {
                    value: RetVal::Prop {
                        var: "b".into(),
                        field: "name".into(),
                    },
                    alias: None,
                },
            ],
            order_by: vec![
                OrderItem {
                    target: OrderTarget::Alias("since".into()),
                    descending: true,
                },
                OrderItem {
                    target: OrderTarget::Prop {
                        var: "b".into(),
                        field: "name".into(),
                    },
                    descending: false,
                },
            ],
            skip: Some(1),
            limit: Some(5),
        };
        assert_eq!(got, expected);
    }

    #[test]
    fn rel_direction_right() {
        let q = parse_src("MATCH (a)-[r:T]->(b) RETURN a").unwrap();
        assert_eq!(q.matches[0].chain[0].0.dir, RelDir::Right);
        assert_eq!(q.matches[0].chain[0].0.var.as_deref(), Some("r"));
        assert_eq!(q.matches[0].chain[0].0.etype.as_deref(), Some("T"));
    }

    #[test]
    fn rel_direction_left() {
        let q = parse_src("MATCH (a)<-[r:T]-(b) RETURN a").unwrap();
        assert_eq!(q.matches[0].chain[0].0.dir, RelDir::Left);
    }

    #[test]
    fn rel_direction_undirected() {
        let q = parse_src("MATCH (a)-[r:T]-(b) RETURN a").unwrap();
        assert_eq!(q.matches[0].chain[0].0.dir, RelDir::Undirected);
    }

    #[test]
    fn node_props_map_with_param() {
        let q = parse_src("MATCH (t:Talent {id: $tid, n: 1, s: 'x'}) RETURN t").unwrap();
        assert_eq!(
            q.matches[0].start.props,
            vec![
                ("id".into(), Operand::Param("tid".into())),
                ("n".into(), Operand::Lit(Value::Int(1))),
                ("s".into(), Operand::Lit(Value::Str("x".into()))),
            ]
        );
        assert_eq!(q.matches[0].start.var.as_deref(), Some("t"));
        assert_eq!(q.matches[0].start.label.as_deref(), Some("Talent"));
    }

    #[test]
    fn operator_precedence_or_and_not() {
        let q = parse_src("MATCH (a) WHERE a.x = 1 OR b.y = 2 AND NOT c.z = 3 RETURN a").unwrap();
        let expected = Expr::Or(
            Box::new(cmp(prop("a", "x"), CmpOp::Eq, Operand::Lit(Value::Int(1)))),
            Box::new(Expr::And(
                Box::new(cmp(prop("b", "y"), CmpOp::Eq, Operand::Lit(Value::Int(2)))),
                Box::new(Expr::Not(Box::new(cmp(
                    prop("c", "z"),
                    CmpOp::Eq,
                    Operand::Lit(Value::Int(3)),
                )))),
            )),
        );
        assert_eq!(q.where_expr, Some(expected));
    }

    #[test]
    fn unary_minus_folds_numeric_literals() {
        let q = parse_src("MATCH (a) WHERE a.x > -5 AND a.y < -1.5 RETURN a").unwrap();
        let expected = Expr::And(
            Box::new(cmp(prop("a", "x"), CmpOp::Gt, Operand::Lit(Value::Int(-5)))),
            Box::new(cmp(
                prop("a", "y"),
                CmpOp::Lt,
                Operand::Lit(Value::Float(-1.5)),
            )),
        );
        assert_eq!(q.where_expr, Some(expected));
    }

    fn assert_parse_err(src: &str) {
        let result = std::panic::catch_unwind(|| parse_src(src));
        assert!(result.is_ok(), "parse({src:?}) panicked");
        let err = result
            .unwrap()
            .expect_err(&format!("parse({src:?}) must be Err"));
        // Parse errors say "token"; lex errors say "position". Either is a valid reject.
        assert!(
            err.contains("token") || err.contains("position"),
            "error must include token or lex position, got: {err}"
        );
    }

    #[test]
    fn malformed_match_alone_is_err() {
        assert_parse_err("MATCH");
    }

    #[test]
    fn malformed_missing_return_is_err() {
        assert_parse_err("MATCH (n)");
    }

    #[test]
    fn malformed_rel_colon_without_type_is_err() {
        assert_parse_err("MATCH (a)-[x:]->(b) RETURN a");
    }

    #[test]
    fn malformed_dangling_comma_in_return_is_err() {
        assert_parse_err("MATCH (n) RETURN n,");
    }

    #[test]
    fn malformed_unclosed_paren_is_err() {
        assert_parse_err("MATCH (n RETURN n");
        assert_parse_err("MATCH (n) WHERE (a.x = 1 RETURN n");
    }

    #[test]
    fn malformed_order_by_before_return_is_err() {
        assert_parse_err("MATCH (n) ORDER BY n RETURN n");
    }

    #[test]
    fn malformed_garbage_after_limit_is_err() {
        assert_parse_err("MATCH (n) RETURN n LIMIT 1 extra");
    }

    #[test]
    fn dogfood_query_exact_ast() {
        let src = "\
MATCH (t:Talent {id: $tid}) \
MATCH (c:Company)-[i:INDUSTRY_ALIGNMENT]->(t) \
MATCH (c)-[s:SPECIALTY_MATCH]->(t) \
WHERE i.score >= 0.5 AND s.score >= 0.5 \
RETURN c, i.score AS industry, s.score AS specialty \
ORDER BY industry DESC, specialty DESC \
LIMIT 10";
        let got = parse_src(src).expect("dogfood query must parse");
        let expected = Query {
            matches: vec![
                Pattern {
                    start: node(
                        Some("t"),
                        Some("Talent"),
                        vec![("id".into(), Operand::Param("tid".into()))],
                    ),
                    chain: vec![],
                    shortest: false,
                },
                Pattern {
                    start: node(Some("c"), Some("Company"), vec![]),
                    chain: vec![(
                        RelPat {
                            var: Some("i".into()),
                            etype: Some("INDUSTRY_ALIGNMENT".into()),
                            dir: RelDir::Right,
                            hops: None,
                        },
                        node(Some("t"), None, vec![]),
                    )],
                    shortest: false,
                },
                Pattern {
                    start: node(Some("c"), None, vec![]),
                    chain: vec![(
                        RelPat {
                            var: Some("s".into()),
                            etype: Some("SPECIALTY_MATCH".into()),
                            dir: RelDir::Right,
                            hops: None,
                        },
                        node(Some("t"), None, vec![]),
                    )],
                    shortest: false,
                },
            ],
            where_expr: Some(Expr::And(
                Box::new(cmp(
                    prop("i", "score"),
                    CmpOp::Ge,
                    Operand::Lit(Value::Float(0.5)),
                )),
                Box::new(cmp(
                    prop("s", "score"),
                    CmpOp::Ge,
                    Operand::Lit(Value::Float(0.5)),
                )),
            )),
            returns: vec![
                RetItem {
                    value: RetVal::Var("c".into()),
                    alias: None,
                },
                RetItem {
                    value: RetVal::Prop {
                        var: "i".into(),
                        field: "score".into(),
                    },
                    alias: Some("industry".into()),
                },
                RetItem {
                    value: RetVal::Prop {
                        var: "s".into(),
                        field: "score".into(),
                    },
                    alias: Some("specialty".into()),
                },
            ],
            order_by: vec![
                OrderItem {
                    target: OrderTarget::Alias("industry".into()),
                    descending: true,
                },
                OrderItem {
                    target: OrderTarget::Alias("specialty".into()),
                    descending: true,
                },
            ],
            skip: None,
            limit: Some(10),
        };
        assert_eq!(got, expected);
    }

    #[test]
    fn unary_minus_in_props_and_dash_elsewhere_is_err() {
        let q = parse_src("MATCH (a {x: -5, y: -1.5}) RETURN a").unwrap();
        assert_eq!(
            q.matches[0].start.props,
            vec![
                ("x".into(), Operand::Lit(Value::Int(-5))),
                ("y".into(), Operand::Lit(Value::Float(-1.5))),
            ]
        );
        assert_parse_err("MATCH (a) WHERE a.x = 1 - 2 RETURN a");
        assert_parse_err("MATCH (a) WHERE a.x > -b.y RETURN a");
        assert_parse_err("MATCH (a) RETURN a SKIP -1");
    }

    #[test]
    fn paren_grouping_and_and_left_assoc() {
        let q = parse_src("MATCH (a) WHERE (a.x = 1 OR a.y = 2) AND a.z = 3 RETURN a").unwrap();
        let expected = Expr::And(
            Box::new(Expr::Or(
                Box::new(cmp(prop("a", "x"), CmpOp::Eq, Operand::Lit(Value::Int(1)))),
                Box::new(cmp(prop("a", "y"), CmpOp::Eq, Operand::Lit(Value::Int(2)))),
            )),
            Box::new(cmp(prop("a", "z"), CmpOp::Eq, Operand::Lit(Value::Int(3)))),
        );
        assert_eq!(q.where_expr, Some(expected));

        let q = parse_src("MATCH (a) WHERE a.x = 1 AND a.y = 2 AND a.z = 3 RETURN a").unwrap();
        let expected = Expr::And(
            Box::new(Expr::And(
                Box::new(cmp(prop("a", "x"), CmpOp::Eq, Operand::Lit(Value::Int(1)))),
                Box::new(cmp(prop("a", "y"), CmpOp::Eq, Operand::Lit(Value::Int(2)))),
            )),
            Box::new(cmp(prop("a", "z"), CmpOp::Eq, Operand::Lit(Value::Int(3)))),
        );
        assert_eq!(q.where_expr, Some(expected));
    }

    #[test]
    fn order_by_bare_ident_is_var_when_not_an_alias() {
        let q = parse_src("MATCH (a) RETURN a, b.name ORDER BY a, b.name").unwrap();
        assert_eq!(
            q.order_by,
            vec![
                OrderItem {
                    target: OrderTarget::Var("a".into()),
                    descending: false,
                },
                OrderItem {
                    target: OrderTarget::Prop {
                        var: "b".into(),
                        field: "name".into(),
                    },
                    descending: false,
                },
            ]
        );
    }

    #[test]
    fn parse_never_panics_on_token_sequences() {
        let sequences: Vec<Vec<Tok>> = vec![
            vec![],
            vec![Tok::Match],
            vec![Tok::Return],
            vec![Tok::Dash, Tok::Dash, Tok::Dash],
            vec![Tok::Lt, Tok::Gt, Tok::Eq],
            vec![Tok::LParen, Tok::RParen, Tok::RParen],
            vec![Tok::Int(1), Tok::Float(2.0), Tok::Str("x".into())],
            vec![Tok::Where, Tok::Not, Tok::And, Tok::Or],
            vec![Tok::Order, Tok::By, Tok::Asc, Tok::Desc],
            vec![Tok::Skip, Tok::Limit, Tok::As],
            vec![Tok::Ident("n".into()), Tok::Dot, Tok::Ident("x".into())],
            vec![Tok::Param("p".into()), Tok::Colon, Tok::Comma],
            vec![Tok::LBracket, Tok::RBracket, Tok::LBrace, Tok::RBrace],
            lex("MATCH (a)-[x:]->(b) RETURN a ORDER BY a LIMIT 1 extra").unwrap(),
            // Aggregate tokens: COUNT(*), SUM/AVG/MIN/MAX in various positions.
            vec![
                Tok::Ident("COUNT".into()),
                Tok::LParen,
                Tok::Star,
                Tok::RParen,
            ],
            vec![
                Tok::Ident("sum".into()),
                Tok::LParen,
                Tok::Ident("n".into()),
                Tok::Dot,
                Tok::Ident("x".into()),
                Tok::RParen,
            ],
            vec![Tok::Star],
            vec![Tok::Star, Tok::LParen, Tok::RParen, Tok::Star],
            vec![
                Tok::Ident("avg".into()),
                Tok::LParen,
                Tok::Star,
                Tok::RParen,
            ],
            vec![Tok::Ident("min".into()), Tok::LParen, Tok::RParen],
            vec![
                Tok::Ident("max".into()),
                Tok::LParen,
                Tok::Star,
                Tok::RParen,
            ],
        ];
        for toks in sequences {
            let result = std::panic::catch_unwind(|| parse(&toks));
            assert!(result.is_ok(), "parse panicked on token sequence {toks:?}");
            let parsed = result.unwrap();
            if let Err(err) = parsed {
                assert!(
                    err.contains("token"),
                    "error must include token position, got: {err}"
                );
            }
        }
    }

    #[test]
    fn aggregate_functions_parse_to_agg_retval() {
        use crate::cypher::ast::{AggArg, AggFunc, RetVal};

        // COUNT(*) → AggFunc::Count, AggArg::Star
        let q = parse_src("MATCH (n) RETURN COUNT(*)").unwrap();
        assert_eq!(q.returns.len(), 1);
        assert_eq!(
            q.returns[0].value,
            RetVal::Agg {
                func: AggFunc::Count,
                arg: AggArg::Star,
            }
        );
        assert_eq!(q.returns[0].alias, None);

        // COUNT(n) → AggFunc::Count, AggArg::Var
        let q = parse_src("MATCH (n) RETURN COUNT(n)").unwrap();
        assert_eq!(
            q.returns[0].value,
            RetVal::Agg {
                func: AggFunc::Count,
                arg: AggArg::Var("n".into()),
            }
        );

        // SUM(n.age) → AggFunc::Sum, AggArg::Prop
        let q = parse_src("MATCH (n) RETURN SUM(n.age) AS total").unwrap();
        assert_eq!(
            q.returns[0].value,
            RetVal::Agg {
                func: AggFunc::Sum,
                arg: AggArg::Prop {
                    var: "n".into(),
                    field: "age".into()
                },
            }
        );
        assert_eq!(q.returns[0].alias, Some("total".into()));

        // AVG, MIN, MAX case-insensitive
        let q = parse_src("MATCH (n) RETURN avg(n.score)").unwrap();
        assert!(matches!(
            q.returns[0].value,
            RetVal::Agg {
                func: AggFunc::Avg,
                ..
            }
        ));
        let q = parse_src("MATCH (n) RETURN Min(n.x)").unwrap();
        assert!(matches!(
            q.returns[0].value,
            RetVal::Agg {
                func: AggFunc::Min,
                ..
            }
        ));
        let q = parse_src("MATCH (n) RETURN MAX(n.x)").unwrap();
        assert!(matches!(
            q.returns[0].value,
            RetVal::Agg {
                func: AggFunc::Max,
                ..
            }
        ));
    }

    #[test]
    fn nested_parens_beyond_limit_is_err_not_panic() {
        let mut src = String::from("MATCH (a) WHERE ");
        for _ in 0..80 {
            src.push('(');
        }
        src.push_str("a.x = 1");
        for _ in 0..80 {
            src.push(')');
        }
        src.push_str(" RETURN a");
        assert_parse_err(&src);
    }

    // ── Variable-length path parser tests ─────────────────────────────────────

    fn hop_range_of(src: &str) -> HopRange {
        let q = parse_src(src).expect(src);
        let (rel, _) = &q.matches[0].chain[0];
        rel.hops.expect("expected hop range")
    }

    fn assert_hop_err(src: &str, needle: &str) {
        let result = std::panic::catch_unwind(|| parse_src(src));
        assert!(result.is_ok(), "parse panicked on {src:?}");
        let err = result
            .unwrap()
            .expect_err(&format!("parse({src:?}) must Err"));
        assert!(
            err.contains(needle),
            "error must contain {needle:?}, got: {err}"
        );
    }

    #[test]
    fn var_length_bare_star_is_one_to_ten() {
        let r = hop_range_of("MATCH (a)-[r:T*]->(b) RETURN a");
        assert_eq!(r, HopRange { min: 1, max: 10 });
    }

    #[test]
    fn var_length_exact_n_hops() {
        let r = hop_range_of("MATCH (a)-[r:T*3]->(b) RETURN a");
        assert_eq!(r, HopRange { min: 3, max: 3 });
    }

    #[test]
    fn var_length_min_max_range() {
        let r = hop_range_of("MATCH (a)-[r:T*2..5]->(b) RETURN a");
        assert_eq!(r, HopRange { min: 2, max: 5 });
    }

    #[test]
    fn var_length_dotdot_max() {
        let r = hop_range_of("MATCH (a)-[r:T*..4]->(b) RETURN a");
        assert_eq!(r, HopRange { min: 1, max: 4 });
    }

    #[test]
    fn var_length_cap_at_ten_is_ok() {
        let r = hop_range_of("MATCH (a)-[r:T*10]->(b) RETURN a");
        assert_eq!(r, HopRange { min: 10, max: 10 });
        let r2 = hop_range_of("MATCH (a)-[r:T*1..10]->(b) RETURN a");
        assert_eq!(r2, HopRange { min: 1, max: 10 });
    }

    #[test]
    fn var_length_cap_exceeded_is_err() {
        assert_hop_err(
            "MATCH (a)-[r:T*11]->(b) RETURN a",
            "variable-length paths are capped at 10 hops",
        );
        assert_hop_err(
            "MATCH (a)-[r:T*1..11]->(b) RETURN a",
            "variable-length paths are capped at 10 hops",
        );
    }

    #[test]
    fn var_length_unbounded_min_dot_dot_is_err() {
        assert_hop_err(
            "MATCH (a)-[r:T*2..]->(b) RETURN a",
            "variable-length paths are capped at 10 hops",
        );
    }

    #[test]
    fn var_length_shortest_path_parses() {
        let q = parse_src(
            "MATCH (a:N) MATCH (b:N) MATCH shortestPath((a)-[r:T*..5]->(b)) RETURN a",
        )
        .expect("shortestPath must parse");
        assert!(q.matches[2].shortest, "third match must be shortest=true");
        let (rel, _) = &q.matches[2].chain[0];
        assert_eq!(rel.hops, Some(HopRange { min: 1, max: 5 }));
        assert_eq!(rel.etype.as_deref(), Some("T"));
    }

    #[test]
    fn var_length_no_type_is_ok() {
        // Bare `*` with no type filter
        let r = hop_range_of("MATCH (a)-[r*1..3]->(b) RETURN a");
        assert_eq!(r, HopRange { min: 1, max: 3 });
    }

    #[test]
    fn var_length_rel_appears_in_chain() {
        let q = parse_src("MATCH (a)-[r:T*2..4]->(b) RETURN a").unwrap();
        let (rel, dest) = &q.matches[0].chain[0];
        assert_eq!(rel.var.as_deref(), Some("r"));
        assert_eq!(rel.etype.as_deref(), Some("T"));
        assert_eq!(rel.dir, RelDir::Right);
        assert_eq!(rel.hops, Some(HopRange { min: 2, max: 4 }));
        assert_eq!(dest.var.as_deref(), Some("b"));
    }
}
