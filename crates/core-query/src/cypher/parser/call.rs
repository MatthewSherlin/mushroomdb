//! Procedure calls (QL-12): `CALL ns.name(args) [YIELD …] [WHERE …] [RETURN …]`
//! as a statement of its own.
//!
//! `CALL` and `YIELD` are ordinary identifiers to the lexer. A statement is a
//! call by its first word only (see [`is_call_tokens`](super::is_call_tokens)),
//! and `YIELD` is a keyword only right after the call's argument list, so both
//! still name properties, labels and variables everywhere else.

use super::{kw, refusal, Parser};
use crate::cypher::ast::{CallReturn, ProcCallStmt, YieldItem};
use crate::cypher::Tok;

impl<'a> Parser<'a> {
    pub(super) fn call_statement(&mut self) -> Result<ProcCallStmt, String> {
        if !self.eat_ident_kw("call") {
            return Err(self.err("expected CALL"));
        }
        // `CALL { … }` and the scoped `CALL (x) { … }` are subqueries.
        if matches!(self.peek(), Some(Tok::LBrace | Tok::LParen)) {
            return Err(refusal("CALL subquery"));
        }
        let (ns, name) = self.procedure_name()?;
        let args = if self.eat(&Tok::LParen) {
            self.call_args()?
        } else {
            Vec::new()
        };
        let yields = if self.eat_ident_kw("yield") {
            self.yield_items()?
        } else {
            None
        };
        let where_expr = if self.eat(&Tok::Where) {
            Some(self.where_expr()?)
        } else {
            None
        };
        let ret = if self.peek() == Some(&Tok::Return) {
            Some(self.call_return()?)
        } else {
            None
        };
        if self.pos < self.toks.len() {
            if self.clause_ahead() {
                return Err(refusal("CALL inside a larger query"));
            }
            return Err(self.err("unexpected tokens after CALL"));
        }
        Ok(ProcCallStmt {
            ns,
            name,
            args,
            yields,
            where_expr,
            ret,
        })
    }

    /// `ns.name`, `ns.sub.name`: at least two dotted parts. The namespace is
    /// every part but the last.
    fn procedure_name(&mut self) -> Result<(String, String), String> {
        const SHAPE: &str = "expected a procedure name of the form namespace.name";
        let mut parts = vec![self.ident(SHAPE)?];
        while self.eat(&Tok::Dot) {
            parts.push(self.ident(SHAPE)?);
        }
        if parts.len() < 2 {
            return Err(self.err(SHAPE));
        }
        let name = parts.pop().unwrap_or_default();
        Ok((parts.join("."), name))
    }

    /// The arguments after the `(`, through the `)`.
    fn call_args(&mut self) -> Result<Vec<crate::cypher::ast::Operand>, String> {
        let mut args = Vec::new();
        if self.eat(&Tok::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.value_expr()?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen, "expected ',' or ')' in procedure arguments")?;
            return Ok(args);
        }
    }

    /// `*`, or `field [AS alias], …` after `YIELD`. `None` for `*`.
    fn yield_items(&mut self) -> Result<Option<Vec<YieldItem>>, String> {
        if self.eat(&Tok::Star) {
            return Ok(None);
        }
        let mut items: Vec<YieldItem> = Vec::new();
        loop {
            let field = self.ident("expected a column name after YIELD")?;
            let alias = if self.eat(&Tok::As) {
                Some(self.binding("expected a name after AS")?)
            } else {
                self.not_reserved(&field)?;
                None
            };
            let item = YieldItem { field, alias };
            if items.iter().any(|seen| seen.name() == item.name()) {
                return Err(format!(
                    "YIELD binds `{}` twice; give one of them another name with AS",
                    item.name()
                ));
            }
            items.push(item);
            if !self.eat(&Tok::Comma) {
                return Ok(Some(items));
            }
        }
    }

    /// `RETURN [DISTINCT] items [ORDER BY …] [SKIP n] [LIMIT n]`.
    fn call_return(&mut self) -> Result<CallReturn, String> {
        let (distinct, items) = self.return_clause()?;
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
        Ok(CallReturn {
            distinct,
            items,
            order_by,
            skip,
            limit,
        })
    }

    /// Whether the next token starts another clause or query part.
    fn clause_ahead(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Tok::Match
                    | Tok::Optional
                    | Tok::Unwind
                    | Tok::With
                    | Tok::Create
                    | Tok::Merge
                    | Tok::Set
                    | Tok::Delete
                    | Tok::Detach
                    | Tok::Return
            )
        ) || ["call", "union", "foreach", "remove", "load"]
            .iter()
            .any(|word| kw(self.peek(), word))
    }
}
