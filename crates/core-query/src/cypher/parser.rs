//! Recursive-descent parser for the Cypher subset. Never panics on any token sequence.

use super::ast::{
    AggArg, AggFunc, ArithOp, CreateEdge, CreateNode, CreateStmt, EdgeDelete, Expr, HopRange,
    LimitSkip, MatchDeleteNodeStmt, MatchDeleteStmt, MatchSetStmt, MergeStmt, NodePat, Operand,
    OptionalClause, OrderItem, OrderTarget, Pattern, Query, RelDir, RelPat, RetItem, RetVal,
    SetClause, UnwindClause, UnwindExpr, WithStage, WriteStatement,
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

/// Parse a tokenized write statement (CREATE / MATCH…SET / MATCH…DELETE / MERGE).
/// Returns `Err` for read queries or malformed write statements.
pub fn parse_write(tokens: &[Tok]) -> Result<WriteStatement, String> {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
    };
    p.write_statement()
}

/// Return true if the token stream starts with a write keyword, or is a MATCH
/// statement followed by SET or DELETE.  Used for fast server-side dispatch
/// without a full parse.
pub fn is_write_tokens(tokens: &[Tok]) -> bool {
    match tokens.first() {
        Some(Tok::Create) | Some(Tok::Merge) => true,
        Some(Tok::Match) => tokens.iter().any(|t| matches!(t, Tok::Set | Tok::Delete)),
        _ => false,
    }
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
        // Optional WHERE clause that follows the required MATCH(es) — may come
        // before or after OPTIONAL MATCH.  In standard Cypher, WHERE applies to
        // the preceding MATCH, so `MATCH (a) WHERE … OPTIONAL MATCH … RETURN`
        // is legal.  Parse it here, then parse OPTIONAL MATCHes, then check
        // again in case WHERE follows the OPTIONAL MATCHes instead.
        let where_expr_pre = if self.eat(&Tok::Where) {
            Some(self.expr(0)?)
        } else {
            None
        };
        // Optional MATCH clauses (zero or more, after required MATCHes + WHERE).
        let mut optional_clauses = Vec::new();
        while self.peek() == Some(&Tok::Optional) {
            optional_clauses.push(self.optional_match_clause()?);
        }
        // WHERE may also appear *after* OPTIONAL MATCHes (if not already seen).
        let where_expr = if where_expr_pre.is_some() {
            where_expr_pre
        } else if self.eat(&Tok::Where) {
            Some(self.expr(0)?)
        } else {
            None
        };
        // Optional top-level UNWIND clauses (after WHERE, before WITH).
        let mut unwinds = Vec::new();
        while self.peek() == Some(&Tok::Unwind) {
            unwinds.push(self.unwind_clause()?);
        }
        // Optional WHERE that follows UNWIND (references UNWIND aliases).
        let post_unwind_where = if !unwinds.is_empty() && self.eat(&Tok::Where) {
            Some(self.expr(0)?)
        } else {
            None
        };
        // WITH pipeline stages (zero or more).
        let mut stages = Vec::new();
        while self.peek() == Some(&Tok::With) {
            stages.push(self.with_stage()?);
        }
        let (distinct, returns) = self.return_clause()?;
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
            return Err(self.unsupported_or_unexpected("unexpected tokens after query"));
        }
        Ok(Query {
            matches,
            optional_clauses,
            where_expr,
            unwinds,
            post_unwind_where,
            stages,
            returns,
            distinct,
            order_by,
            skip,
            limit,
        })
    }

    /// Named errors for still-unsupported Cypher forms (`UNION`, `CASE`).
    fn unsupported_or_unexpected(&self, msg: &str) -> String {
        match self.peek() {
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("union") => {
                "UNION is not supported".to_string()
            }
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("case") => {
                "CASE is not supported".to_string()
            }
            _ => self.err(msg),
        }
    }

    /// Consume an identifier keyword (case-insensitive). Keywords that are
    /// not lexer tokens (`IN`, `DISTINCT`, `ON`) stay `Ident` so they can
    /// still be used as variable names in other positions.
    fn eat_ident_kw(&mut self, kw: &str) -> bool {
        if let Some(Tok::Ident(s)) = self.peek() {
            if s.eq_ignore_ascii_case(kw) {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    /// Parse one `OPTIONAL MATCH pattern [WHERE expr]` clause.
    ///
    /// Standard openCypher allows exactly one MATCH per OPTIONAL MATCH.
    fn optional_match_clause(&mut self) -> Result<OptionalClause, String> {
        self.expect(&Tok::Optional, "expected OPTIONAL")?;
        self.expect(&Tok::Match, "expected MATCH after OPTIONAL")?;
        let patterns = vec![self.pattern()?];
        // Optional WHERE inside the optional scope.
        let where_expr = if self.eat(&Tok::Where) {
            Some(self.expr(0)?)
        } else {
            None
        };
        Ok(OptionalClause {
            patterns,
            where_expr,
        })
    }

    /// Parse one `WITH <items> [WHERE] [ORDER BY] [SKIP] [LIMIT] [MATCH]* [UNWIND]* [WHERE]`
    /// stage and return a `WithStage`.
    fn with_stage(&mut self) -> Result<WithStage, String> {
        self.expect(&Tok::With, "expected WITH")?;
        let mut items = vec![self.ret_item()?];
        while self.eat(&Tok::Comma) {
            items.push(self.ret_item()?);
        }
        // Optional WHERE / HAVING immediately after WITH items.
        let where_expr = if self.eat(&Tok::Where) {
            Some(self.expr(0)?)
        } else {
            None
        };
        // Optional ORDER BY inside WITH.
        let aliases: Vec<&str> = items.iter().filter_map(|r| r.alias.as_deref()).collect();
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
        // Optional MATCH clauses that follow this WITH.
        let mut matches = Vec::new();
        while self.peek() == Some(&Tok::Match) {
            matches.push(self.match_clause()?);
        }
        // Optional OPTIONAL MATCH clauses that follow those MATCHes.
        let mut optional_clauses = Vec::new();
        while self.peek() == Some(&Tok::Optional) {
            optional_clauses.push(self.optional_match_clause()?);
        }
        // Optional UNWIND clauses that follow those MATCHes.
        let mut stage_unwinds = Vec::new();
        while self.peek() == Some(&Tok::Unwind) {
            stage_unwinds.push(self.unwind_clause()?);
        }
        // Optional WHERE that follows those MATCHes / UNWINDs.
        let post_where = if self.peek() == Some(&Tok::Where)
            && (!matches.is_empty() || !optional_clauses.is_empty() || !stage_unwinds.is_empty())
        {
            self.pos += 1; // consume WHERE
            Some(self.expr(0)?)
        } else {
            None
        };
        Ok(WithStage {
            items,
            where_expr,
            order_by,
            skip,
            limit,
            matches,
            optional_clauses,
            unwinds: stage_unwinds,
            post_where,
        })
    }

    /// Parse `UNWIND <expr> AS <alias>`.
    fn unwind_clause(&mut self) -> Result<UnwindClause, String> {
        self.expect(&Tok::Unwind, "expected UNWIND")?;
        let list = self.unwind_expr()?;
        self.expect(&Tok::As, "expected AS after UNWIND expression")?;
        let alias = self.ident("expected alias identifier after AS")?;
        Ok(UnwindClause { list, alias })
    }

    /// Parse the list expression in an UNWIND clause:
    /// - `[v1, v2, …]` — literal list.
    /// - `var.field`   — property reference.
    /// - `var`         — bare variable (alias from a prior WITH).
    fn unwind_expr(&mut self) -> Result<UnwindExpr, String> {
        match self.peek() {
            Some(Tok::LBracket) => {
                self.pos += 1; // consume '['
                let mut vals = Vec::new();
                if !self.eat(&Tok::RBracket) {
                    loop {
                        vals.push(self.literal_value("UNWIND list element")?);
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        self.expect(&Tok::RBracket, "expected ']' to close UNWIND list")?;
                        break;
                    }
                }
                Ok(UnwindExpr::Lit(vals))
            }
            Some(Tok::Ident(_)) => {
                let name = self.ident("expected variable or property in UNWIND")?;
                if self.eat(&Tok::Dot) {
                    let field = self.ident("expected field name after '.' in UNWIND")?;
                    Ok(UnwindExpr::Prop { var: name, field })
                } else {
                    Ok(UnwindExpr::Var(name))
                }
            }
            _ => Err(self.err("expected list literal, property, or variable in UNWIND")),
        }
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
        self.expect(&Tok::LParen, "expected '(' after shortestPath")?;
        let start = self.node()?;
        let rel = self.rel()?;
        if rel.hops.is_none() {
            return Err(
                self.err("shortestPath requires a variable-length relationship (e.g. [*..5])")
            );
        }
        let dest = self.node()?;
        self.expect(&Tok::RParen, "expected ')' to close shortestPath")?;
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
            let (var, etypes, hops) = self.rel_body()?;
            self.expect(
                &Tok::Dash,
                "expected '-' to close a left-directed relationship",
            )?;
            return Ok(RelPat {
                var,
                etypes,
                dir: RelDir::Left,
                hops,
            });
        }
        self.expect(&Tok::Dash, "expected '-' to start a relationship")?;
        let (var, etypes, hops) = self.rel_body()?;
        self.expect(&Tok::Dash, "expected '-' after ']'")?;
        let dir = if self.eat(&Tok::Gt) {
            RelDir::Right
        } else {
            RelDir::Undirected
        };
        Ok(RelPat {
            var,
            etypes,
            dir,
            hops,
        })
    }

    #[allow(clippy::type_complexity)]
    fn rel_body(&mut self) -> Result<(Option<String>, Vec<String>, Option<HopRange>), String> {
        self.expect(&Tok::LBracket, "expected '[' in a relationship pattern")?;
        let var = match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.pos += 1;
                Some(s)
            }
            _ => None,
        };
        // `:A`, or `:A|:B|:C` alternation. Empty = any type.
        let mut etypes = Vec::new();
        if self.eat(&Tok::Colon) {
            etypes.push(self.ident("expected relationship type identifier after ':'")?);
            while self.eat(&Tok::Pipe) {
                // Both `|:B` (openCypher) and the lenient `|B` are accepted.
                self.eat(&Tok::Colon);
                etypes.push(self.ident("expected relationship type identifier after '|'")?);
            }
        }
        let hops = if self.eat(&Tok::Star) {
            Some(self.parse_hop_range()?)
        } else {
            None
        };
        self.expect(
            &Tok::RBracket,
            "expected ']' to close a relationship pattern",
        )?;
        Ok((var, etypes, hops))
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
                    _ => Err(self.err("expected max-hop integer after '*..'")),
                }
            }

            // Anything else after `*` — treat as bare `*`
            _ => Ok(HopRange { min: 1, max: 10 }),
        }
    }

    fn validate_hop_range(
        &self,
        min_n: i64,
        max_n: i64,
        cap_err: &str,
    ) -> Result<HopRange, String> {
        if min_n < 0 || max_n < 0 {
            return Err(self.err("hop counts must be non-negative"));
        }
        if min_n == 0 {
            return Err(self.err(
                "zero-length variable-length paths are not supported; minimum hop count is 1",
            ));
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
        let lhs = self.arith_expr()?;
        // Check for IS NULL / IS NOT NULL postfix.
        if let Some(Tok::Ident(s)) = self.peek() {
            if s.eq_ignore_ascii_case("is") {
                self.pos += 1; // consume "IS"
                               // Optional NOT.
                let negated = self.eat(&Tok::Not);
                // Expect NULL identifier.
                match self.peek() {
                    Some(Tok::Ident(n)) if n.eq_ignore_ascii_case("null") => {
                        self.pos += 1; // consume "NULL"
                        return Ok(if negated {
                            Expr::IsNotNull(lhs)
                        } else {
                            Expr::IsNull(lhs)
                        });
                    }
                    _ => {
                        return Err(self.err(if negated {
                            "expected NULL after IS NOT"
                        } else {
                            "expected NULL after IS"
                        }));
                    }
                }
            }
        }
        // `IN [a, b, $p]` or `IN $list`.
        if self.eat_ident_kw("in") {
            let list = self.in_list()?;
            return Ok(Expr::In { expr: lhs, list });
        }
        // If no comparison operator follows, treat the operand as a standalone
        // boolean predicate (Expr::Truthy).  This enables:
        //   WHERE textMatches(n.bio, 'query')
        // without requiring an explicit `= true` or similar.
        match self.cmp_op() {
            Ok(op) => {
                let rhs = self.arith_expr()?;
                Ok(Expr::Cmp { lhs, op, rhs })
            }
            Err(_) => Ok(Expr::Truthy(lhs)),
        }
    }

    /// Parse the list operand of `IN`: `[a, b, $p]` or a single operand (`$cities`).
    fn in_list(&mut self) -> Result<Vec<Operand>, String> {
        if self.eat(&Tok::LBracket) {
            let mut items = Vec::new();
            if !self.eat(&Tok::RBracket) {
                loop {
                    items.push(self.arith_expr()?);
                    if self.eat(&Tok::Comma) {
                        continue;
                    }
                    self.expect(&Tok::RBracket, "expected ']' to close IN list")?;
                    break;
                }
            }
            Ok(items)
        } else {
            Ok(vec![self.arith_expr()?])
        }
    }

    // ── Arithmetic expression parsing (precedence: * / > + -) ──────────────
    //
    // Grammar:
    //   arith_expr  = arith_add
    //   arith_add   = arith_mul ((+ | -) arith_mul)*
    //   arith_mul   = arith_unary ((* | /) arith_unary)*
    //   arith_unary = - arith_atom | arith_atom
    //   arith_atom  = literal | param | ident | ident.field | ident(args…) | (arith_add)

    /// Parse a full arithmetic expression (additive level).
    fn arith_expr(&mut self) -> Result<Operand, String> {
        let mut left = self.arith_mul()?;
        loop {
            let op = if self.eat(&Tok::Plus) {
                ArithOp::Add
            } else if self.eat(&Tok::Dash) {
                // Dash is `-`; but we must not consume a Dash that starts a
                // relationship pattern (those appear at the top-level pattern
                // parser, not inside an expression). Inside expressions `-`
                // is always subtraction.
                ArithOp::Sub
            } else {
                break;
            };
            let right = self.arith_mul()?;
            left = Operand::BinArith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Parse a multiplicative-level arithmetic expression.
    fn arith_mul(&mut self) -> Result<Operand, String> {
        let mut left = self.arith_unary()?;
        loop {
            let op = if self.eat(&Tok::Star) {
                ArithOp::Mul
            } else if self.eat(&Tok::Slash) {
                ArithOp::Div
            } else {
                break;
            };
            let right = self.arith_unary()?;
            left = Operand::BinArith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Parse a unary-level arithmetic expression (handles unary `-`).
    fn arith_unary(&mut self) -> Result<Operand, String> {
        if self.eat(&Tok::Dash) {
            // Unary minus: fold into the next atom.
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
        self.arith_atom()
    }

    /// Parse an atomic operand (leaf of the arithmetic expression tree).
    fn arith_atom(&mut self) -> Result<Operand, String> {
        // Parenthesized arithmetic expression.
        if self.eat(&Tok::LParen) {
            let inner = self.arith_expr()?;
            self.expect(&Tok::RParen, "expected ')' to close arithmetic expression")?;
            return Ok(inner);
        }
        // Delegate to the existing atom parser (no unary minus here — handled above).
        self.operand_atom()
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

    /// Parse a single atomic operand (no arithmetic wrapping — use `arith_expr`
    /// for full expression support).  This is the leaf parser for literals,
    /// parameters, property references, variable references, and function calls.
    fn operand_atom(&mut self) -> Result<Operand, String> {
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
                let name = self.ident("expected identifier")?;
                if name.eq_ignore_ascii_case("case") {
                    return self.parse_case();
                }
                if name.eq_ignore_ascii_case("collect") && self.peek() == Some(&Tok::LParen) {
                    // collect() is a top-level RETURN/WITH aggregate (handled in
                    // ret_item); it is not valid inside a larger expression.
                    return Err(
                        "collect() is only supported as a top-level RETURN/WITH aggregate"
                            .to_string(),
                    );
                }
                if self.peek() == Some(&Tok::LParen) {
                    // Scalar function call: name(arg, ...)
                    // Function arguments may be arbitrary arithmetic expressions.
                    self.pos += 1; // consume '('
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::RParen) {
                        args.push(self.arith_expr()?);
                        while self.eat(&Tok::Comma) {
                            args.push(self.arith_expr()?);
                        }
                    }
                    self.expect(&Tok::RParen, "expected ')' to close function call")?;
                    Ok(Operand::FuncCall { name, args })
                } else if self.eat(&Tok::Dot) {
                    let field = self.ident("expected field name after '.'")?;
                    Ok(Operand::Prop { var: name, field })
                } else {
                    // Bare variable reference (e.g. alias name in WITH … WHERE c > 2).
                    Ok(Operand::Var(name))
                }
            }
            _ => Err(self.err("expected operand (property, literal, or parameter)")),
        }
    }

    /// Parse a generic `CASE WHEN <cond> THEN <value> … [ELSE <value>] END`.
    /// The `CASE` keyword has already been consumed. `WHEN`/`THEN`/`ELSE`/`END`
    /// are ordinary identifiers to the lexer, matched case-insensitively.
    fn parse_case(&mut self) -> Result<Operand, String> {
        let is_kw = |tok: Option<&Tok>, kw: &str| matches!(tok, Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw));
        let mut branches = Vec::new();
        while is_kw(self.peek(), "when") {
            self.pos += 1; // consume WHEN
            let cond = self.expr(0)?;
            if !is_kw(self.peek(), "then") {
                return Err(self.err("expected THEN in a CASE branch"));
            }
            self.pos += 1; // consume THEN
            let value = self.arith_expr()?;
            branches.push((cond, value));
        }
        if branches.is_empty() {
            return Err(self.err("CASE requires at least one WHEN ... THEN branch"));
        }
        let default = if is_kw(self.peek(), "else") {
            self.pos += 1; // consume ELSE
            Some(Box::new(self.arith_expr()?))
        } else {
            None
        };
        if !is_kw(self.peek(), "end") {
            return Err(self.err("expected END to close CASE"));
        }
        self.pos += 1; // consume END
        Ok(Operand::Case { branches, default })
    }

    /// Parse a full arithmetic expression (additive + multiplicative + unary +
    /// atom).  This is the primary operand entry point for WHERE, RETURN, SET,
    /// and function arguments.  Use `operand_atom` for contexts that truly
    /// require a single atom (e.g., MATCH property map values where arithmetic
    /// would be syntactically ambiguous with the `}` delimiter).
    fn operand(&mut self) -> Result<Operand, String> {
        // In contexts where `operand` is called for MATCH props, the parser
        // never sees arithmetic operators (`+`/`/`) because they can't appear
        // inside `{key: val}` maps.  Delegating to `arith_expr` is safe here.
        self.arith_expr()
    }

    fn return_clause(&mut self) -> Result<(bool, Vec<RetItem>), String> {
        if !self.eat(&Tok::Return) {
            return Err(self.unsupported_or_unexpected("expected RETURN"));
        }
        let distinct = self.eat_ident_kw("distinct");
        let mut items = vec![self.ret_item()?];
        while self.eat(&Tok::Comma) {
            items.push(self.ret_item()?);
        }
        Ok((distinct, items))
    }

    fn return_items(&mut self) -> Result<Vec<RetItem>, String> {
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
                "collect" => Some(AggFunc::Collect),
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

        // Non-aggregate RETURN item: parse as a full arithmetic expression,
        // then convert the resulting Operand to the appropriate RetVal variant.
        // This unified path handles:
        //   n          → RetVal::Var
        //   n.prop     → RetVal::Prop
        //   f(...)     → RetVal::FuncCall
        //   n.age + 1  → RetVal::ScalarExpr(BinArith)
        //   42         → RetVal::ScalarExpr(Lit)
        let op = self.arith_expr()?;
        let value = match op {
            Operand::Var(name) => RetVal::Var(name),
            Operand::Prop { var, field } => RetVal::Prop { var, field },
            Operand::FuncCall { name, args } => RetVal::FuncCall { name, args },
            other => RetVal::ScalarExpr(other),
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

    fn uint(&mut self, what: &str) -> Result<LimitSkip, String> {
        match self.peek() {
            Some(Tok::Int(n)) if *n >= 0 => {
                let n = *n as u64;
                self.pos += 1;
                Ok(LimitSkip::Exact(n))
            }
            Some(Tok::Int(_)) => Err(self.err(&format!("{what} must be a non-negative integer"))),
            Some(Tok::Param(_)) => {
                let name = match self.toks.get(self.pos) {
                    Some(Tok::Param(s)) => s.clone(),
                    _ => unreachable!(),
                };
                self.pos += 1;
                Ok(LimitSkip::Param(name))
            }
            _ => Err(self.err(&format!("expected integer or $parameter after {what}"))),
        }
    }

    // ── Write statement parsing ───────────────────────────────────────────────

    fn write_statement(&mut self) -> Result<WriteStatement, String> {
        match self.peek() {
            Some(Tok::Create) => self.create_stmt(),
            Some(Tok::Merge) => self.merge_stmt(),
            Some(Tok::Match) => self.match_write_stmt(),
            _ => Err(self
                .err("expected CREATE, MERGE, or MATCH … SET/DELETE (write statement required)")),
        }
    }

    // ── CREATE ────────────────────────────────────────────────────────────────

    fn create_stmt(&mut self) -> Result<WriteStatement, String> {
        self.expect(&Tok::Create, "expected CREATE")?;
        let mut stmt = self.create_pattern()?;
        // Optional RETURN clause: `CREATE (n:L {…}) RETURN n` or `RETURN n.id AS id`.
        if self.eat(&Tok::Return) {
            stmt.returns = Some(self.return_items()?);
        }
        if self.pos < self.toks.len() {
            return Err(self.err("unexpected tokens after CREATE"));
        }
        Ok(WriteStatement::Create(stmt))
    }

    fn create_pattern(&mut self) -> Result<CreateStmt, String> {
        // Parse the first (possibly only) node.
        let first = self.create_node(0)?;
        let first_var = first.var.clone().unwrap_or_else(|| format!("_cn{}", 0));
        let mut nodes: Vec<CreateNode> = vec![first];
        let mut edges: Vec<CreateEdge> = Vec::new();

        // Chain: (-[:T]-> | <-[:T]-) followed by another node.
        while matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
            let (etype, src_is_left) = self.create_rel()?;
            let idx = nodes.len();
            let next = self.create_node(idx)?;
            let next_var = next.var.clone().unwrap_or_else(|| format!("_cn{idx}"));
            let prev_var = nodes.last().unwrap().var.clone().unwrap_or_else(|| {
                if nodes.len() == 1 {
                    first_var.clone()
                } else {
                    format!("_cn{}", nodes.len() - 1)
                }
            });
            let (src_var, dst_var) = if src_is_left {
                // <-[:T]- means next→prev i.e. next is src
                (next_var.clone(), prev_var)
            } else {
                // -[:T]-> means prev→next
                (prev_var, next_var.clone())
            };
            edges.push(CreateEdge {
                src_var,
                etype,
                dst_var,
            });
            nodes.push(next);
        }
        Ok(CreateStmt {
            nodes,
            edges,
            returns: None,
        })
    }

    fn create_node(&mut self, idx: usize) -> Result<CreateNode, String> {
        self.expect(&Tok::LParen, "expected '(' in CREATE node pattern")?;
        let var = match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.pos += 1;
                Some(s)
            }
            _ => None,
        };
        if !self.eat(&Tok::Colon) {
            return Err(self.err("CREATE node requires a label (e.g., (n:Label {…}))"));
        }
        let label = self.ident("expected label identifier after ':'")?;
        let props = if self.peek() == Some(&Tok::LBrace) {
            self.literal_props()?
        } else {
            Vec::new()
        };
        self.expect(&Tok::RParen, "expected ')' to close CREATE node pattern")?;
        let var = Some(var.unwrap_or_else(|| format!("_cn{idx}")));
        Ok(CreateNode { var, label, props })
    }

    /// Parse `{key: literal, …}` where all values must be literals (no params, no props).
    fn literal_props(&mut self) -> Result<Vec<(String, Value)>, String> {
        self.expect(&Tok::LBrace, "expected '{'")?;
        let mut out = Vec::new();
        if self.eat(&Tok::RBrace) {
            return Ok(out);
        }
        loop {
            let key = self.ident("expected property key")?;
            self.expect(&Tok::Colon, "expected ':' after property key")?;
            let val = self.literal_value("property value")?;
            out.push((key, val));
            if self.eat(&Tok::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Tok::RBrace, "expected '}' to close property map")?;
        Ok(out)
    }

    /// Parse a literal value (int, float, or string). Parameters and property
    /// references are not accepted in write statements (v1 limitation).
    fn literal_value(&mut self, what: &str) -> Result<Value, String> {
        if self.eat(&Tok::Dash) {
            return match self.peek() {
                Some(Tok::Int(n)) => {
                    let n = *n;
                    self.pos += 1;
                    Ok(Value::Int(-n))
                }
                Some(Tok::Float(x)) => {
                    let x = *x;
                    self.pos += 1;
                    Ok(Value::Float(-x))
                }
                _ => Err(self.err("unary minus only applies to numeric literals")),
            };
        }
        match self.peek() {
            Some(Tok::Int(n)) => {
                let n = *n;
                self.pos += 1;
                Ok(Value::Int(n))
            }
            Some(Tok::Float(x)) => {
                let x = *x;
                self.pos += 1;
                Ok(Value::Float(x))
            }
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(Value::Str(s))
            }
            // List literal: `[]`, `[v1, v2, …]`, nesting allowed. Elements are
            // themselves literal values (recursion), matching the UNWIND list
            // form and downstream `Value::List` support in the store.
            Some(Tok::LBracket) => {
                self.pos += 1; // consume '['
                let mut items = Vec::new();
                if !self.eat(&Tok::RBracket) {
                    loop {
                        items.push(self.literal_value(what)?);
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        self.expect(&Tok::RBracket, "expected ']' to close list literal")?;
                        break;
                    }
                }
                Ok(Value::List(items))
            }
            Some(Tok::Param(_)) => Err(self.err(&format!(
                "parameter references are not supported in {what} (v1 limitation: use literals only)"
            ))),
            Some(Tok::Ident(_)) => Err(self.err(&format!(
                "expression RHS not supported in {what} (v1 limitation: use literals only)"
            ))),
            _ => Err(self.err(&format!("expected literal value for {what}"))),
        }
    }

    /// Parse `-[:TYPE]->` or `<-[:TYPE]-`.  Returns `(etype, src_is_left)` where
    /// `src_is_left = true` means left node is dst (i.e., next node is src).
    fn create_rel(&mut self) -> Result<(String, bool), String> {
        if self.eat(&Tok::Lt) {
            // <-[:TYPE]-
            self.expect(&Tok::Dash, "expected '-' after '<' in relationship")?;
            self.expect(&Tok::LBracket, "expected '[' in relationship pattern")?;
            self.expect(&Tok::Colon, "expected ':TYPE' in CREATE relationship")?;
            let etype = self.ident("expected relationship type")?;
            self.expect(&Tok::RBracket, "expected ']'")?;
            self.expect(&Tok::Dash, "expected '-'")?;
            return Ok((etype, true));
        }
        // -[:TYPE]->
        self.expect(&Tok::Dash, "expected '-' to start relationship")?;
        self.expect(&Tok::LBracket, "expected '[' in relationship pattern")?;
        self.expect(&Tok::Colon, "expected ':TYPE' in CREATE relationship")?;
        let etype = self.ident("expected relationship type")?;
        self.expect(&Tok::RBracket, "expected ']'")?;
        self.expect(&Tok::Dash, "expected '-'")?;
        self.expect(
            &Tok::Gt,
            "expected '>' — CREATE requires directed relationships",
        )?;
        Ok((etype, false))
    }

    // ── MERGE ─────────────────────────────────────────────────────────────────

    fn merge_stmt(&mut self) -> Result<WriteStatement, String> {
        self.expect(&Tok::Merge, "expected MERGE")?;
        self.expect(&Tok::LParen, "expected '(' after MERGE")?;
        // Optional var: `MERGE (n:Label {…})` — capture n for RETURN projection.
        let var = match self.peek() {
            Some(Tok::Ident(_)) => {
                let s = match self.toks.get(self.pos) {
                    Some(Tok::Ident(s)) => s.clone(),
                    _ => unreachable!(),
                };
                self.pos += 1; // consume var name
                Some(s)
            }
            _ => None,
        };
        if !self.eat(&Tok::Colon) {
            return Err(self.err("MERGE requires a label (e.g., MERGE (n:Label {key: 'x'}))"));
        }
        let label = self.ident("expected label identifier after ':'")?;
        if self.peek() != Some(&Tok::LBrace) {
            return Err(self.err(
                "MERGE requires a property map with exactly one key (e.g., MERGE (n:Label {id: 'x'}))",
            ));
        }
        let mut props = self.literal_props()?;
        if props.len() != 1 {
            return Err(format!(
                "MERGE supports exactly one key property (got {}); use CREATE for multi-prop nodes",
                props.len()
            ));
        }
        self.expect(&Tok::RParen, "expected ')' to close MERGE pattern")?;
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        while self.eat_ident_kw("on") {
            if self.eat(&Tok::Create) {
                self.expect(&Tok::Set, "expected SET after ON CREATE")?;
                on_create.extend(self.set_clauses()?);
            } else if self.eat(&Tok::Match) {
                self.expect(&Tok::Set, "expected SET after ON MATCH")?;
                on_match.extend(self.set_clauses()?);
            } else {
                return Err(self.err("expected CREATE or MATCH after ON"));
            }
        }
        // Optional RETURN clause.
        let returns = if self.eat(&Tok::Return) {
            Some(self.return_items()?)
        } else {
            None
        };
        if self.pos < self.toks.len() {
            return Err(self.unsupported_or_unexpected("unexpected tokens after MERGE"));
        }
        let (key_field, key_value) = props.remove(0);
        Ok(WriteStatement::Merge(MergeStmt {
            label,
            key_field,
            key_value,
            var,
            on_create,
            on_match,
            returns,
        }))
    }

    // ── MATCH … SET / MATCH … DELETE ─────────────────────────────────────────

    fn match_write_stmt(&mut self) -> Result<WriteStatement, String> {
        // Parse MATCH clauses (same as read query).
        let mut matches = Vec::new();
        while self.peek() == Some(&Tok::Match) {
            matches.push(self.match_clause()?);
        }
        if matches.is_empty() {
            return Err(self.err("expected MATCH"));
        }
        // Optional WHERE.
        let where_expr = if self.eat(&Tok::Where) {
            Some(self.expr(0)?)
        } else {
            None
        };
        // Dispatch on SET, DETACH DELETE, or DELETE.
        match self.peek() {
            Some(Tok::Set) => {
                self.pos += 1; // consume SET
                let sets = self.set_clauses()?;
                let returns = if self.eat(&Tok::Return) {
                    Some(self.return_items()?)
                } else {
                    None
                };
                if self.pos < self.toks.len() {
                    return Err(self.unsupported_or_unexpected("unexpected tokens after SET"));
                }
                Ok(WriteStatement::MatchSet(MatchSetStmt {
                    matches,
                    where_expr,
                    sets,
                    returns,
                }))
            }
            Some(Tok::Detach) => {
                // DETACH DELETE <node_var> [, …]
                self.pos += 1; // consume DETACH
                self.expect(&Tok::Delete, "expected DELETE after DETACH")?;
                let node_vars = self.node_delete_targets(&matches)?;
                if self.pos < self.toks.len() {
                    return Err(self.err("unexpected tokens after DETACH DELETE"));
                }
                Ok(WriteStatement::MatchDeleteNode(MatchDeleteNodeStmt {
                    matches,
                    where_expr,
                    node_vars,
                    detach: true,
                }))
            }
            Some(Tok::Delete) => {
                self.pos += 1; // consume DELETE
                               // Try to resolve all targets as edge vars first. If the first
                               // target is a node var (not an edge var), fall through to node delete.
                match self.delete_targets_or_node(&matches)? {
                    DeleteTargetResult::Edges(deletes) => {
                        if self.pos < self.toks.len() {
                            return Err(self.err("unexpected tokens after DELETE"));
                        }
                        Ok(WriteStatement::MatchDelete(MatchDeleteStmt {
                            matches,
                            where_expr,
                            deletes,
                        }))
                    }
                    DeleteTargetResult::Nodes(node_vars) => {
                        if self.pos < self.toks.len() {
                            return Err(self.err("unexpected tokens after DELETE"));
                        }
                        Ok(WriteStatement::MatchDeleteNode(MatchDeleteNodeStmt {
                            matches,
                            where_expr,
                            node_vars,
                            detach: false,
                        }))
                    }
                }
            }
            _ => Err(self.err(
                "expected SET or DELETE after MATCH [WHERE]; \
                 combined MATCH…RETURN is a read query, not a write statement",
            )),
        }
    }

    fn set_clauses(&mut self) -> Result<Vec<SetClause>, String> {
        let mut sets = vec![self.set_clause()?];
        while self.eat(&Tok::Comma) {
            sets.push(self.set_clause()?);
        }
        Ok(sets)
    }

    fn set_clause(&mut self) -> Result<SetClause, String> {
        let var = self.ident("expected variable in SET clause")?;
        self.expect(&Tok::Dot, "expected '.' after variable in SET")?;
        let field = self.ident("expected field name after '.'")?;
        self.expect(&Tok::Eq, "expected '=' in SET clause")?;
        // Accept Lit, Param, BinArith (arithmetic expression), and FuncCall on
        // the RHS. Bare Prop/Var (e.g. `SET n.x = m.y`) remain a named error
        // because that form requires join semantics not supported in v1; use
        // an arithmetic expression like `m.y + 0` if needed.
        let value = match self.peek() {
            Some(
                Tok::Int(_)
                | Tok::Float(_)
                | Tok::Str(_)
                | Tok::Param(_)
                | Tok::Dash
                | Tok::Ident(_),
            ) => {
                let op = self.arith_expr()?;
                match &op {
                    Operand::Lit(_)
                    | Operand::Param(_)
                    | Operand::BinArith { .. }
                    | Operand::FuncCall { .. }
                    | Operand::Case { .. } => op,
                    Operand::Prop { .. } | Operand::Var(_) => {
                        return Err(self.err(
                            "SET RHS: bare property/variable reference is not supported; \
                             use a literal, $parameter, or arithmetic expression (e.g. n.x + 1)",
                        ));
                    }
                }
            }
            // List-literal RHS: `SET n.tags = ['a', 'b']`. Lists are pure
            // literals (no arithmetic), so parse directly into `Operand::Lit`.
            Some(Tok::LBracket) => Operand::Lit(self.literal_value("SET value")?),
            _ => {
                return Err(
                    self.err("expected literal, $parameter, or arithmetic expression as SET value")
                )
            }
        };
        Ok(SetClause { var, field, value })
    }

    /// Find the rel-var `var` in `patterns` and return its etype, src node var,
    /// and dst node var.  Returns Err if the var is not a rel var or has no type.
    fn resolve_edge_var(&self, var: &str, patterns: &[Pattern]) -> Result<EdgeDelete, String> {
        for pat in patterns {
            let start_var = pat.start.var.as_deref().unwrap_or("_unknown");
            let mut from_var = start_var;
            for (rel, dest) in &pat.chain {
                let to_var = dest.var.as_deref().unwrap_or("_unknown");
                if rel.var.as_deref() == Some(var) {
                    let etype = match rel.etypes.as_slice() {
                        [t] => t.clone(),
                        [] => {
                            return Err(format!(
                                "DELETE `{var}`: relationship has no type; \
                                 DELETE requires an explicit edge type (e.g., [r:TYPE])"
                            ))
                        }
                        _ => {
                            return Err(format!(
                                "DELETE `{var}`: relationship has multiple types; \
                                 DELETE requires a single explicit edge type (e.g., [r:TYPE])"
                            ))
                        }
                    };
                    let (src_var, dst_var) = match rel.dir {
                        RelDir::Right => (from_var.to_string(), to_var.to_string()),
                        RelDir::Left => (to_var.to_string(), from_var.to_string()),
                        RelDir::Undirected => {
                            return Err(format!(
                                "DELETE `{var}`: undirected relationship DELETE is not supported; \
                                 use a directed pattern (e.g., -[r:TYPE]->)"
                            ))
                        }
                    };
                    return Ok(EdgeDelete {
                        rel_var: var.to_string(),
                        etype,
                        src_var,
                        dst_var,
                    });
                }
                from_var = to_var;
            }
        }
        Err(format!(
            "DELETE `{var}`: variable is not bound as a relationship in any MATCH pattern; \
             only relationship variables can be deleted (DELETE edge vars, not node vars)"
        ))
    }

    /// Return `true` if `var` is bound as a node variable in `patterns`.
    fn is_node_var(&self, var: &str, patterns: &[Pattern]) -> bool {
        for pat in patterns {
            if pat.start.var.as_deref() == Some(var) {
                return true;
            }
            for (_, dest) in &pat.chain {
                if dest.var.as_deref() == Some(var) {
                    return true;
                }
            }
        }
        false
    }

    /// Parse a comma-separated list of node-variable targets for
    /// `[DETACH] DELETE`.  All targets must be node variables bound in `patterns`.
    fn node_delete_targets(&mut self, patterns: &[Pattern]) -> Result<Vec<String>, String> {
        let mut vars = Vec::new();
        loop {
            let var = self.ident("expected node variable to DELETE")?;
            if !self.is_node_var(&var, patterns) {
                return Err(format!(
                    "DELETE `{var}`: variable is not bound as a node in any MATCH pattern"
                ));
            }
            vars.push(var);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(vars)
    }

    /// Try to parse DELETE targets as edge vars; if the first target is a node
    /// var, fall back to parsing all targets as node vars.
    fn delete_targets_or_node(
        &mut self,
        patterns: &[Pattern],
    ) -> Result<DeleteTargetResult, String> {
        // Peek at the identifier to decide which path to take.
        let var = self.ident("expected variable to DELETE")?;
        // Try edge var first.
        match self.resolve_edge_var(&var, patterns) {
            Ok(edge_del) => {
                // At least the first target is an edge var; parse the rest as
                // edge vars too.
                let mut targets = vec![edge_del];
                while self.eat(&Tok::Comma) {
                    let v = self.ident("expected variable to DELETE")?;
                    targets.push(self.resolve_edge_var(&v, patterns)?);
                }
                Ok(DeleteTargetResult::Edges(targets))
            }
            Err(_) => {
                // Not an edge var; try as node var.
                if self.is_node_var(&var, patterns) {
                    let mut node_vars = vec![var];
                    while self.eat(&Tok::Comma) {
                        let v = self.ident("expected variable to DELETE")?;
                        if !self.is_node_var(&v, patterns) {
                            return Err(format!(
                                "DELETE `{v}`: variable is not bound as a node in any MATCH pattern"
                            ));
                        }
                        node_vars.push(v);
                    }
                    Ok(DeleteTargetResult::Nodes(node_vars))
                } else {
                    Err(format!(
                        "DELETE `{var}`: variable is not bound as a relationship or node \
                         in any MATCH pattern"
                    ))
                }
            }
        }
    }
}

/// Used internally by `delete_targets_or_node` to signal whether the targets
/// resolved to edge variables or node variables.
enum DeleteTargetResult {
    Edges(Vec<EdgeDelete>),
    Nodes(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::cypher::ast::{
        AggFunc, Expr, HopRange, LimitSkip, NodePat, Operand, OrderItem, OrderTarget, Pattern,
        Query, RelDir, RelPat, RetItem, RetVal,
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
                            etypes: vec!["KNOWS".into()],
                            dir: RelDir::Right,
                            hops: None,
                        },
                        node(Some("b"), None, vec![]),
                    ),
                    (
                        RelPat {
                            var: Some("u".into()),
                            etypes: vec!["TEAM".into()],
                            dir: RelDir::Undirected,
                            hops: None,
                        },
                        node(Some("c"), None, vec![]),
                    ),
                    (
                        RelPat {
                            var: Some("s".into()),
                            etypes: vec!["LIKES".into()],
                            dir: RelDir::Left,
                            hops: None,
                        },
                        node(Some("d"), None, vec![]),
                    ),
                ],
                shortest: false,
            }],
            optional_clauses: vec![],
            unwinds: vec![],
            post_unwind_where: None,
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
            stages: vec![],
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
            distinct: false,
            skip: Some(LimitSkip::Exact(1)),
            limit: Some(LimitSkip::Exact(5)),
        };
        assert_eq!(got, expected);
    }

    #[test]
    fn rel_direction_right() {
        let q = parse_src("MATCH (a)-[r:T]->(b) RETURN a").unwrap();
        assert_eq!(q.matches[0].chain[0].0.dir, RelDir::Right);
        assert_eq!(q.matches[0].chain[0].0.var.as_deref(), Some("r"));
        assert_eq!(q.matches[0].chain[0].0.etypes, vec!["T".to_string()]);
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
                            etypes: vec!["INDUSTRY_ALIGNMENT".into()],
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
                            etypes: vec!["SPECIALTY_MATCH".into()],
                            dir: RelDir::Right,
                            hops: None,
                        },
                        node(Some("t"), None, vec![]),
                    )],
                    shortest: false,
                },
            ],
            optional_clauses: vec![],
            unwinds: vec![],
            post_unwind_where: None,
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
            stages: vec![],
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
            distinct: false,
            skip: None,
            limit: Some(LimitSkip::Exact(10)),
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
        // `1 - 2` is now valid arithmetic — no longer a parse error.
        let q2 = parse_src("MATCH (a) WHERE a.x = 1 - 2 RETURN a").unwrap();
        assert!(q2.where_expr.is_some());
        // Unary minus on non-literal remains an error.
        assert_parse_err("MATCH (a) WHERE a.x > -b.y RETURN a");
        assert_parse_err("MATCH (a) RETURN a SKIP -1");
    }

    // ── IS NULL / IS NOT NULL ──────────────────────────────────────────────────

    #[test]
    fn is_null_parses_on_prop() {
        let q = parse_src("MATCH (a) WHERE a.x IS NULL RETURN a").unwrap();
        assert_eq!(q.where_expr, Some(Expr::IsNull(prop("a", "x"))),);
    }

    #[test]
    fn is_not_null_parses_on_prop() {
        let q = parse_src("MATCH (a) WHERE a.x IS NOT NULL RETURN a").unwrap();
        assert_eq!(q.where_expr, Some(Expr::IsNotNull(prop("a", "x"))),);
    }

    #[test]
    fn is_null_on_var() {
        let q =
            parse_src("MATCH (a) OPTIONAL MATCH (a)-[:T]->(b) WITH a, b WHERE b IS NULL RETURN a")
                .unwrap();
        // The IS NULL filter lives on the first WITH stage's where_expr.
        let stage = &q.stages[0];
        assert_eq!(
            stage.where_expr,
            Some(Expr::IsNull(Operand::Var("b".into()))),
        );
    }

    #[test]
    fn is_null_case_insensitive() {
        let q = parse_src("MATCH (a) WHERE a.x is null RETURN a").unwrap();
        assert_eq!(q.where_expr, Some(Expr::IsNull(prop("a", "x"))));
        let q2 = parse_src("MATCH (a) WHERE a.x IS NOT NULL RETURN a").unwrap();
        assert_eq!(q2.where_expr, Some(Expr::IsNotNull(prop("a", "x"))));
    }

    #[test]
    fn is_null_combined_with_and() {
        let q = parse_src("MATCH (a) WHERE a.x IS NULL AND a.y > 5 RETURN a").unwrap();
        assert!(matches!(q.where_expr, Some(Expr::And(_, _))));
    }

    // ── Arithmetic expression parsing ──────────────────────────────────────────

    #[test]
    fn arith_add_in_where() {
        use crate::cypher::ast::ArithOp;
        let q = parse_src("MATCH (n) WHERE n.age + 1 > 5 RETURN n").unwrap();
        let expected_lhs = Operand::BinArith {
            op: ArithOp::Add,
            left: Box::new(prop("n", "age")),
            right: Box::new(Operand::Lit(Value::Int(1))),
        };
        assert_eq!(
            q.where_expr,
            Some(Expr::Cmp {
                lhs: expected_lhs,
                op: CmpOp::Gt,
                rhs: Operand::Lit(Value::Int(5)),
            })
        );
    }

    #[test]
    fn arith_precedence_mul_over_add() {
        use crate::cypher::ast::ArithOp;
        // 1 + 2 * 3  should parse as  1 + (2 * 3)
        let q = parse_src("MATCH (n) WHERE n.x = 1 + 2 * 3 RETURN n").unwrap();
        let expected_rhs = Operand::BinArith {
            op: ArithOp::Add,
            left: Box::new(Operand::Lit(Value::Int(1))),
            right: Box::new(Operand::BinArith {
                op: ArithOp::Mul,
                left: Box::new(Operand::Lit(Value::Int(2))),
                right: Box::new(Operand::Lit(Value::Int(3))),
            }),
        };
        assert_eq!(
            q.where_expr,
            Some(Expr::Cmp {
                lhs: prop("n", "x"),
                op: CmpOp::Eq,
                rhs: expected_rhs,
            })
        );
    }

    #[test]
    fn arith_parens_override_precedence() {
        use crate::cypher::ast::ArithOp;
        // In RETURN position: (1+2)*3 should parse as (1+2)*3
        let q = parse_src("MATCH (n) RETURN (1 + 2) * 3 AS r").unwrap();
        let expected = RetVal::ScalarExpr(Operand::BinArith {
            op: ArithOp::Mul,
            left: Box::new(Operand::BinArith {
                op: ArithOp::Add,
                left: Box::new(Operand::Lit(Value::Int(1))),
                right: Box::new(Operand::Lit(Value::Int(2))),
            }),
            right: Box::new(Operand::Lit(Value::Int(3))),
        });
        assert_eq!(q.returns[0].value, expected);
        assert_eq!(q.returns[0].alias, Some("r".into()));
    }

    #[test]
    fn arith_scalar_expr_in_return() {
        use crate::cypher::ast::ArithOp;
        let q = parse_src("MATCH (n) RETURN n.age + 1 AS adjusted").unwrap();
        let expected = RetVal::ScalarExpr(Operand::BinArith {
            op: ArithOp::Add,
            left: Box::new(prop("n", "age")),
            right: Box::new(Operand::Lit(Value::Int(1))),
        });
        assert_eq!(q.returns[0].value, expected);
        assert_eq!(q.returns[0].alias, Some("adjusted".into()));
    }

    #[test]
    fn arith_div_in_where() {
        use crate::cypher::ast::ArithOp;
        let q = parse_src("MATCH (n) WHERE n.x / 2 > 3 RETURN n").unwrap();
        assert!(matches!(
            q.where_expr,
            Some(Expr::Cmp {
                lhs: Operand::BinArith {
                    op: ArithOp::Div,
                    ..
                },
                ..
            })
        ));
    }

    // ── CREATE...RETURN and MERGE...RETURN parser tests ────────────────────────

    #[test]
    fn create_return_parses_node_var() {
        use super::parse_write;
        use crate::cypher::ast::{RetVal, WriteStatement};

        let toks = crate::cypher::lex("CREATE (n:Thing {id: 'x'}) RETURN n").unwrap();
        let stmt = parse_write(&toks).unwrap();
        match stmt {
            WriteStatement::Create(s) => {
                assert_eq!(s.nodes.len(), 1);
                let returns = s.returns.expect("expected RETURN clause");
                assert_eq!(returns.len(), 1);
                assert_eq!(returns[0].value, RetVal::Var("n".into()));
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn create_return_prop_with_alias() {
        use super::parse_write;
        use crate::cypher::ast::{RetVal, WriteStatement};

        let toks = crate::cypher::lex("CREATE (n:Thing {id: 'x'}) RETURN n.id AS node_id").unwrap();
        let stmt = parse_write(&toks).unwrap();
        match stmt {
            WriteStatement::Create(s) => {
                let returns = s.returns.expect("RETURN required");
                assert_eq!(
                    returns[0].value,
                    RetVal::Prop {
                        var: "n".into(),
                        field: "id".into()
                    }
                );
                assert_eq!(returns[0].alias, Some("node_id".into()));
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn merge_return_parses_node_var() {
        use super::parse_write;
        use crate::cypher::ast::{RetVal, WriteStatement};

        let toks = crate::cypher::lex("MERGE (n:Thing {id: 'x'}) RETURN n").unwrap();
        let stmt = parse_write(&toks).unwrap();
        match stmt {
            WriteStatement::Merge(s) => {
                assert_eq!(s.var, Some("n".into()));
                let returns = s.returns.expect("RETURN required");
                assert_eq!(returns[0].value, RetVal::Var("n".into()));
            }
            _ => panic!("expected Merge"),
        }
    }

    #[test]
    fn is_write_tokens_still_true_for_create_return() {
        use super::is_write_tokens;
        use crate::cypher::lex;

        let toks = lex("CREATE (n:T {id: 'x'}) RETURN n").unwrap();
        assert!(
            is_write_tokens(&toks),
            "CREATE...RETURN must still be classified as write"
        );
    }

    #[test]
    fn where_in_list_and_param_parses() {
        let q = parse_src("MATCH (n) WHERE n.city IN ['Austin', $c] RETURN n").unwrap();
        match q.where_expr {
            Some(Expr::In { expr, list }) => {
                assert_eq!(
                    expr,
                    Operand::Prop {
                        var: "n".into(),
                        field: "city".into()
                    }
                );
                assert_eq!(list.len(), 2);
                assert_eq!(list[0], Operand::Lit(Value::Str("Austin".into())));
                assert_eq!(list[1], Operand::Param("c".into()));
            }
            other => panic!("expected Expr::In, got {other:?}"),
        }
        let q2 = parse_src("MATCH (n) WHERE n.city IN $cities RETURN n").unwrap();
        match q2.where_expr {
            Some(Expr::In { list, .. }) => {
                assert_eq!(list, vec![Operand::Param("cities".into())]);
            }
            other => panic!("expected Expr::In, got {other:?}"),
        }
    }

    #[test]
    fn return_distinct_parses() {
        let q = parse_src("MATCH (n) RETURN DISTINCT n.city").unwrap();
        assert!(q.distinct);
        assert_eq!(q.returns.len(), 1);
    }

    #[test]
    fn union_is_a_named_error() {
        let err = parse_src("MATCH (n) RETURN n UNION MATCH (m) RETURN m").unwrap_err();
        assert!(
            err.contains("UNION"),
            "UNION must be a named error, got: {err}"
        );
    }

    #[test]
    fn case_when_expression_parses() {
        let q = parse_src("MATCH (n) RETURN CASE WHEN n.x = 1 THEN 2 ELSE 3 END AS c")
            .expect("CASE parses");
        assert!(matches!(
            q.returns[0].value,
            RetVal::ScalarExpr(Operand::Case { .. })
        ));
    }

    #[test]
    fn collect_is_a_supported_aggregate() {
        let q = parse_src("MATCH (n) RETURN collect(n.name) AS names").expect("collect parses");
        assert!(matches!(
            q.returns[0].value,
            RetVal::Agg {
                func: AggFunc::Collect,
                ..
            }
        ));
    }

    #[test]
    fn match_set_return_parses() {
        use super::parse_write;
        use crate::cypher::ast::{RetVal, WriteStatement};

        let toks = crate::cypher::lex("MATCH (n {id:'a'}) SET n.x = 2 RETURN n.x").unwrap();
        let stmt = parse_write(&toks).unwrap();
        match stmt {
            WriteStatement::MatchSet(s) => {
                let returns = s.returns.expect("RETURN required");
                assert_eq!(
                    returns[0].value,
                    RetVal::Prop {
                        var: "n".into(),
                        field: "x".into()
                    }
                );
            }
            other => panic!("expected MatchSet, got {other:?}"),
        }
    }

    #[test]
    fn merge_on_create_and_on_match_parse() {
        use super::parse_write;
        use crate::cypher::ast::WriteStatement;

        let toks = crate::cypher::lex(
            "MERGE (n:L {id:'new'}) ON CREATE SET n.born = 1 ON MATCH SET n.hit = 1 RETURN n",
        )
        .unwrap();
        let stmt = parse_write(&toks).unwrap();
        match stmt {
            WriteStatement::Merge(s) => {
                assert_eq!(s.on_create.len(), 1);
                assert_eq!(s.on_create[0].field, "born");
                assert_eq!(s.on_match.len(), 1);
                assert_eq!(s.on_match[0].field, "hit");
                assert!(s.returns.is_some());
            }
            other => panic!("expected Merge, got {other:?}"),
        }
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
        let q =
            parse_src("MATCH (a:N) MATCH (b:N) MATCH shortestPath((a)-[r:T*..5]->(b)) RETURN a")
                .expect("shortestPath must parse");
        assert!(q.matches[2].shortest, "third match must be shortest=true");
        let (rel, _) = &q.matches[2].chain[0];
        assert_eq!(rel.hops, Some(HopRange { min: 1, max: 5 }));
        assert_eq!(rel.etypes, vec!["T".to_string()]);
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
        assert_eq!(rel.etypes, vec!["T".to_string()]);
        assert_eq!(rel.dir, RelDir::Right);
        assert_eq!(rel.hops, Some(HopRange { min: 2, max: 4 }));
        assert_eq!(dest.var.as_deref(), Some("b"));
    }

    #[test]
    fn var_length_zero_hop_minimum_is_err() {
        // `*0` — exact form with min=0
        assert_hop_err(
            "MATCH (a)-[r:T*0]->(b) RETURN a",
            "zero-length variable-length paths are not supported",
        );
        // `*0..3` — range form with min=0
        assert_hop_err(
            "MATCH (a)-[r:T*0..3]->(b) RETURN a",
            "zero-length variable-length paths are not supported",
        );
    }

    #[test]
    fn create_accepts_list_literal_property() {
        use super::parse_write;
        use crate::cypher::ast::WriteStatement;
        let src = "CREATE (n:Person {id: 'p1', tags: ['a', 'b']})";
        let stmt = parse_write(&lex(src).unwrap())
            .expect("CREATE with a list-literal property must parse");
        let WriteStatement::Create(c) = stmt else {
            panic!("expected a Create statement");
        };
        let tags = &c.nodes[0]
            .props
            .iter()
            .find(|(k, _)| k == "tags")
            .expect("tags property present")
            .1;
        assert_eq!(
            *tags,
            Value::List(vec![Value::Str("a".into()), Value::Str("b".into())])
        );
    }

    #[test]
    fn create_accepts_empty_and_nested_list_literals() {
        use super::parse_write;
        use crate::cypher::ast::WriteStatement;
        let src = "CREATE (n:L {id: 'p1', empty: [], nested: [[1, 2], [3]]})";
        let stmt = parse_write(&lex(src).unwrap()).expect("empty and nested lists must parse");
        let WriteStatement::Create(c) = stmt else {
            panic!("expected a Create statement");
        };
        let get = |k: &str| {
            c.nodes[0]
                .props
                .iter()
                .find(|(name, _)| name == k)
                .expect("property present")
                .1
                .clone()
        };
        assert_eq!(get("empty"), Value::List(vec![]));
        assert_eq!(
            get("nested"),
            Value::List(vec![
                Value::List(vec![Value::Int(1), Value::Int(2)]),
                Value::List(vec![Value::Int(3)]),
            ])
        );
    }

    #[test]
    fn set_accepts_list_literal_rhs() {
        use super::parse_write;
        use crate::cypher::ast::{Operand, WriteStatement};
        let src = "MATCH (n:Person {id: 'p1'}) SET n.tags = ['x', 'y']";
        let stmt = parse_write(&lex(src).unwrap()).expect("SET with a list-literal RHS must parse");
        let WriteStatement::MatchSet(m) = stmt else {
            panic!("expected a MatchSet statement");
        };
        let set = &m.sets[0];
        assert_eq!(set.field, "tags");
        assert_eq!(
            set.value,
            Operand::Lit(Value::List(vec![
                Value::Str("x".into()),
                Value::Str("y".into())
            ]))
        );
    }
}
