//! Recursive-descent parser for the Cypher subset. Never panics on any token sequence.

use super::ast::{
    AggArg, AggFunc, ArithOp, CreateEdge, CreateNode, CreateStmt, DdlStatement, EdgeDelete, Expr,
    HopRange, LimitSkip, ListPredKind, MatchDeleteNodeStmt, MatchDeleteStmt, MatchRemoveStmt,
    MatchSetMapStmt, MatchSetStmt, MergeEdgePat, MergeStmt, NodePat, Operand, OptionalClause,
    OrderItem, OrderTarget, Pattern, ProcCallStmt, Query, RelDir, RelPat, RetItem, RetVal,
    SetClause, UnwindClause, UnwindExpr, WithStage, WriteStatement, LABEL_TEST_FN, ONE_ROW_VAR,
};
use unwind_write::{Scope, VarKind};
/// A parsed `SET var = {…}` / `SET var += {…}` clause: the variable, whether it
/// is the merge (`+=`) form, and the map's entries.
type SetMapClause = (String, bool, Vec<(String, Operand)>, Option<String>);

use super::Tok;
use crate::filter::CmpOp;
use core_storage::{Value, NS_PROP};

/// Max parenthesized-expression nesting. Deeper input is `Err`, not a stack overflow.
const MAX_PAREN_DEPTH: usize = 64;

/// Max nesting of expressions of any kind (parentheses, function arguments,
/// list and map elements, CASE branches), counted together. Deeper input is
/// `Err`, not a stack overflow. Measured at 63 levels: about 1.1 MiB of stack in
/// a debug build (16-18 KiB a level) and 0.3 MiB in a release build, so the
/// limit fits a 2 MiB thread.
const MAX_EXPR_DEPTH: usize = MAX_PAREN_DEPTH;

/// Parse a tokenized Cypher subset query. Every failure is `Err(String)`; this
/// function never panics on a well-formed `&[Tok]` (including empty / garbage).
pub fn parse(tokens: &[Tok]) -> Result<Query, String> {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        literal_depth: 0,
        expr_depth: 0,
    };
    let q = p.query()?;
    if p.pos < p.toks.len() {
        // A single-query parse: a trailing `UNION` (or anything else) is an
        // error here. Use `parse_read` to accept a `UNION` chain.
        return Err(p.unsupported_or_unexpected("unexpected tokens after query"));
    }
    Ok(q)
}

/// A read query that may be a single query or a `UNION` / `UNION ALL` chain.
/// `all_flags[i]` is `true` when the boundary before `parts[i + 1]` was
/// `UNION ALL` (bag union); `false` for `UNION` (distinct). Length is
/// `parts.len() - 1`.
#[derive(Debug, Clone, PartialEq)]
pub struct UnionQuery {
    pub parts: Vec<Query>,
    pub all_flags: Vec<bool>,
}

/// True if `tok` is the identifier `word`, compared case-insensitively.
pub(crate) fn kw(tok: Option<&Tok>, word: &str) -> bool {
    matches!(tok, Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(word))
}

/// Parse a read query, accepting a `UNION` / `UNION ALL` chain.
pub fn parse_read(tokens: &[Tok]) -> Result<UnionQuery, String> {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        literal_depth: 0,
        expr_depth: 0,
    };
    let mut parts = vec![p.query()?];
    let mut all_flags = Vec::new();
    while p.peek_is_union() {
        p.pos += 1; // consume UNION
        let all = matches!(p.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("all"));
        if all {
            p.pos += 1; // consume ALL
        }
        all_flags.push(all);
        parts.push(p.query()?);
    }
    if p.pos < p.toks.len() {
        return Err(p.err("unexpected tokens after UNION query"));
    }
    Ok(UnionQuery { parts, all_flags })
}

/// Parse a tokenized write statement (CREATE / MATCH…SET / MATCH…DELETE / MERGE).
/// Returns `Err` for read queries or malformed write statements.
pub fn parse_write(tokens: &[Tok]) -> Result<WriteStatement, String> {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        literal_depth: 0,
        expr_depth: 0,
    };
    p.write_statement()
}

/// Parse a procedure call statement, `CALL ns.name(args) [YIELD …] [WHERE …]
/// [RETURN …]` (QL-12). Callers tell a call apart with [`is_call_tokens`]
/// first. `CALL { … }` is refused as a subquery, and a call followed by any
/// other clause as a call inside a larger query.
pub fn parse_call(tokens: &[Tok]) -> Result<ProcCallStmt, String> {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        literal_depth: 0,
        expr_depth: 0,
    };
    p.call_statement()
}

/// Whether the tokens begin a `CALL` statement. No read or write statement
/// can begin with the word, so a malformed call is reported as one.
pub fn is_call_tokens(tokens: &[Tok]) -> bool {
    kw(tokens.first(), "call")
}

/// The error for an openCypher form this release recognises and does not run.
pub(crate) fn refusal(feature: &str) -> String {
    format!("not supported in mushroomdb 0.10: {feature}")
}

/// Parse a schema statement (`CREATE INDEX`, `DROP INDEX`, `SHOW INDEXES`,
/// `CREATE CONSTRAINT`, `DROP CONSTRAINT`, `SHOW CONSTRAINTS`). The statement
/// must be the whole input. Callers tell DDL apart with [`is_ddl_tokens`]
/// first, so a malformed DDL statement gets this parser's error rather than a
/// read or write parser's.
pub fn parse_ddl(tokens: &[Tok]) -> Result<DdlStatement, String> {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        literal_depth: 0,
        expr_depth: 0,
    };
    p.ddl_statement()
}

/// Whether the tokens begin a schema statement: `CREATE INDEX`,
/// `CREATE CONSTRAINT`, or anything starting with `DROP` or `SHOW` — no read
/// or write statement can begin with either word, so a malformed one is
/// reported as malformed DDL.
pub fn is_ddl_tokens(tokens: &[Tok]) -> bool {
    match tokens.first() {
        Some(Tok::Create) => kw(tokens.get(1), "index") || kw(tokens.get(1), "constraint"),
        first => kw(first, "drop") || kw(first, "show"),
    }
}

/// Return true if the token stream starts with a write keyword, or is a MATCH
/// statement followed by SET or DELETE.  Used for fast server-side dispatch
/// without a full parse.
///
/// Schema statements that change the schema (`CREATE INDEX|CONSTRAINT`,
/// `DROP …`) are writes; `SHOW …` is a read.
pub fn is_write_tokens(tokens: &[Tok]) -> bool {
    match tokens.first() {
        Some(Tok::Create) | Some(Tok::Merge) => true,
        first if kw(first, "drop") => true,
        // `MATCH … SET|DELETE|REMOVE|CREATE|MERGE …`.
        Some(Tok::Match) => {
            tokens
                .iter()
                .any(|t| matches!(t, Tok::Set | Tok::Delete | Tok::Create | Tok::Merge))
                || has_remove_clause(tokens)
        }
        // `UNWIND … CREATE|MERGE|SET|DELETE|REMOVE …`: a write when any write
        // clause follows; `UNWIND … RETURN` stays a read.
        Some(Tok::Unwind) => {
            tokens.iter().any(|t| {
                matches!(
                    t,
                    Tok::Create | Tok::Merge | Tok::Set | Tok::Delete | Tok::Detach
                )
            }) || has_remove_clause(tokens)
        }
        _ => false,
    }
}

/// Whether a statement that begins with `CREATE` or `MERGE` needs the per-row
/// form (QL-PM): a `$parameter` inside a node pattern (`{id: $id}`, `(n:P
/// $props)`), or a map `SET` (`ON CREATE SET n += $props`, `SET n = {…}`).
/// The per-row form evaluates its values against the statement's parameters,
/// and runs once, over one row, when no clause before it produces more. A
/// statement with neither keeps the plain form, which folds its literals
/// while parsing.
fn needs_per_row_form(tokens: &[Tok]) -> bool {
    let mut depth = 0usize;
    // Before the first `ON CREATE` / `ON MATCH`: the node patterns.
    let mut in_patterns = true;
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            Tok::LParen => depth += 1,
            Tok::RParen => depth = depth.saturating_sub(1),
            Tok::Return if depth == 0 => return false,
            Tok::Ident(_) if depth == 0 && kw(Some(tok), "on") => in_patterns = false,
            Tok::Param(_) if depth > 0 && in_patterns => return true,
            // `SET n = …` / `SET n += …`, or the same item after a comma.
            Tok::Set | Tok::Comma
                if depth == 0
                    && matches!(tokens.get(i + 1), Some(Tok::Ident(_)))
                    && matches!(
                        (tokens.get(i + 2), tokens.get(i + 3)),
                        (Some(Tok::Eq), _) | (Some(Tok::Plus), Some(Tok::Eq))
                    ) =>
            {
                return true
            }
            _ => {}
        }
    }
    false
}

/// True if the statement writes a relationship's properties, or names a
/// relationship a `CREATE` makes (QL-RP): a property map or `$parameter`
/// inside a `-[ … ]-` bracket, a `CREATE … -[r…]-` variable, or a `SET` /
/// `REMOVE` item whose target is a relationship variable. Such a statement is
/// parsed as the per-row form, the one executor that writes relationship
/// properties.
fn writes_relationship_properties(tokens: &[Tok]) -> bool {
    let mut rel_vars: Vec<&str> = Vec::new();
    let mut in_create = false;
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Tok::Create => in_create = true,
            Tok::Match | Tok::Merge | Tok::With | Tok::Unwind | Tok::Return | Tok::Set => {
                in_create = false
            }
            Tok::LBracket if i > 0 && tokens[i - 1] == Tok::Dash => {
                if let Some(Tok::Ident(name)) = tokens.get(i + 1) {
                    if in_create {
                        return true;
                    }
                    rel_vars.push(name);
                }
                let mut j = i + 1;
                while j < tokens.len() && tokens[j] != Tok::RBracket {
                    if matches!(tokens[j], Tok::LBrace | Tok::Param(_)) {
                        return true;
                    }
                    j += 1;
                }
                i = j;
            }
            _ => {}
        }
        i += 1;
    }
    if rel_vars.is_empty() {
        return false;
    }
    // A SET / REMOVE item's target: the name right after `SET`, `REMOVE` or a
    // comma of that clause, outside any bracket.
    let mut depth = 0usize;
    let mut in_items = false;
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
            Tok::RParen | Tok::RBracket | Tok::RBrace => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            Tok::Set => in_items = true,
            _ if kw(Some(tok), "remove") => in_items = true,
            Tok::Comma if in_items => {}
            Tok::Ident(name) if in_items => {
                let target = i > 0
                    && (matches!(tokens[i - 1], Tok::Set | Tok::Comma)
                        || kw(tokens.get(i - 1), "remove"));
                if target && rel_vars.contains(&name.as_str()) {
                    return true;
                }
            }
            Tok::Create
            | Tok::Merge
            | Tok::Match
            | Tok::With
            | Tok::Unwind
            | Tok::Return
            | Tok::Delete
            | Tok::Detach => in_items = false,
            _ if kw(Some(tok), "on") => in_items = false,
            _ => {}
        }
    }
    false
}

/// True if the tokens hold a `REMOVE var.field` or `REMOVE var:Label` clause.
/// `REMOVE` is an ordinary identifier, so it counts only in clause position:
/// followed by `name` and then `.` or `:`, and not itself a property or label
/// name (`n.remove`, `(x:Remove)`).
fn has_remove_clause(tokens: &[Tok]) -> bool {
    tokens.iter().enumerate().any(|(i, tok)| {
        kw(Some(tok), "remove")
            && !matches!(
                i.checked_sub(1).and_then(|j| tokens.get(j)),
                Some(Tok::Dot | Tok::Colon)
            )
            && matches!(tokens.get(i + 1), Some(Tok::Ident(_)))
            && matches!(tokens.get(i + 2), Some(Tok::Dot | Tok::Colon))
    })
}

/// `true` or `false` in any case, as the literal it spells; `None` for any
/// other identifier. A name followed by `.` or `(` is a property reference or
/// a function call and never reaches this.
fn bool_literal(name: &str) -> Option<bool> {
    if name.eq_ignore_ascii_case("true") {
        Some(true)
    } else if name.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

/// `null` in any case. Like `true` and `false` it is recognised here, not by
/// the lexer, so `n.null` and a backtick-quoted `` `null` `` key stay names.
fn is_null_literal(name: &str) -> bool {
    name.eq_ignore_ascii_case("null")
}

/// A predicate in a value position.
fn pred(expr: Expr) -> Operand {
    Operand::Pred(Box::new(expr))
}

/// The quantifier a function name spells, in any case.
fn list_pred_kind(name: &str) -> Option<ListPredKind> {
    [
        ("all", ListPredKind::All),
        ("any", ListPredKind::Any),
        ("none", ListPredKind::None),
        ("single", ListPredKind::Single),
    ]
    .into_iter()
    .find(|(n, _)| name.eq_ignore_ascii_case(n))
    .map(|(_, kind)| kind)
}

/// The predicate an expression stands for in a WHERE: a predicate is itself;
/// any other value is read for its truthiness.
fn into_predicate(op: Operand) -> Expr {
    match op {
        Operand::Pred(expr) => *expr,
        other => Expr::Truthy(other),
    }
}

/// Reads a `{key: value, …}` map whose values are of type `V`.
type PropsParser<'a, V> = fn(&mut Parser<'a>) -> Result<Vec<(String, V)>, String>;

/// A parsed MERGE node pattern.
struct MergeNode<V = Value> {
    var: Option<String>,
    label: String,
    key_field: String,
    key_value: V,
    ns: Option<V>,
}

mod call;
mod ddl;
mod unwind_write;

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    /// How many list/map literals enclose the position being parsed.
    literal_depth: usize,
    /// How many expressions (parentheses, arguments, elements, branches)
    /// enclose the position being parsed.
    expr_depth: usize,
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
        if let Some(refused) = self.unsupported_form() {
            return refused;
        }
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

    /// `true`, `false` and `null` are literals, so none can name a variable or
    /// an alias: an expression that mentions the name would read the literal.
    fn not_reserved(&self, name: &str) -> Result<(), String> {
        if bool_literal(name).is_some() {
            return Err(self.err(&format!(
                "`{name}` is a boolean literal and cannot name a variable or an alias"
            )));
        }
        if is_null_literal(name) {
            return Err(self.err(&format!(
                "`{name}` is the null literal and cannot name a variable or an alias"
            )));
        }
        Ok(())
    }

    /// An identifier that binds a name: an alias after `AS`.
    fn binding(&mut self, what: &str) -> Result<String, String> {
        if let Some(Tok::Ident(s)) = self.peek() {
            self.not_reserved(s)?;
        }
        self.ident(what)
    }

    fn query(&mut self) -> Result<Query, String> {
        let mut matches = Vec::new();
        while self.peek() == Some(&Tok::Match) {
            matches.extend(self.match_clause()?);
        }
        // A leading OPTIONAL MATCH reads as `UNWIND [1] AS <ONE_ROW_VAR>
        // OPTIONAL MATCH …`: it runs against one row, so it gives one row of
        // nulls when nothing matches.
        let leading_optional = matches.is_empty() && self.peek() == Some(&Tok::Optional);
        // A query may also open with UNWIND, WITH or RETURN; it then reads
        // nothing from the graph and runs against one empty row.
        if matches.is_empty()
            && !leading_optional
            && !matches!(
                self.peek(),
                Some(Tok::Unwind) | Some(Tok::With) | Some(Tok::Return)
            )
        {
            return Err(self.err("expected MATCH, OPTIONAL MATCH, UNWIND, WITH or RETURN"));
        }
        let mut scope = Scope::default();
        let mut unwinds = Vec::new();
        let mut optional_clauses = Vec::new();
        let mut where_expr = None;
        if leading_optional {
            scope.bind_hidden(ONE_ROW_VAR, VarKind::Scalar);
            unwinds.push(unwind_write::one_row_unwind());
        } else {
            // Optional WHERE clause that follows the required MATCH(es) — may
            // come before or after OPTIONAL MATCH.  In standard Cypher, WHERE
            // applies to the preceding MATCH, so `MATCH (a) WHERE … OPTIONAL
            // MATCH … RETURN` is legal.  Parse it here, then parse OPTIONAL
            // MATCHes, then check again in case WHERE follows the OPTIONAL
            // MATCHes instead.
            if self.eat(&Tok::Where) {
                where_expr = Some(self.where_expr()?);
            }
            // Optional MATCH clauses (zero or more, after required MATCHes + WHERE).
            while self.peek() == Some(&Tok::Optional) {
                optional_clauses.push(self.optional_match_clause()?);
            }
            // WHERE may also appear *after* OPTIONAL MATCHes (if not already seen).
            if where_expr.is_none() && self.eat(&Tok::Where) {
                where_expr = Some(self.where_expr()?);
            }
        }
        // Optional top-level UNWIND clauses (after WHERE, before WITH).
        while self.peek() == Some(&Tok::Unwind) {
            unwinds.push(self.unwind_clause()?);
        }
        // Optional WHERE that follows UNWIND (references UNWIND aliases).
        let post_unwind_where = if !unwinds.is_empty() && self.eat(&Tok::Where) {
            Some(self.where_expr()?)
        } else {
            None
        };
        // WITH pipeline stages (zero or more). After an UNWIND the stages may
        // also be plain MATCH / OPTIONAL MATCH / UNWIND clauses.
        let mut stages = Vec::new();
        scope.bind_clauses(&matches, &optional_clauses, &unwinds);
        if unwinds.is_empty() {
            while self.peek() == Some(&Tok::With) {
                let stage = self.with_stage(&scope)?;
                scope.after_with(&stage.items);
                scope.bind_clauses(&stage.matches, &stage.optional_clauses, &stage.unwinds);
                stages.push(stage);
            }
        } else {
            self.stages_after_unwind(&mut scope, &mut stages)?;
        }
        let (distinct, returns) = self.return_clause(&scope)?;
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
        // NOTE: the leftover-token check lives in `parse` / `parse_read`, not
        // here, so `query()` can stop cleanly at a `UNION` boundary.
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

    /// True if the next token is the `UNION` keyword (an ordinary identifier).
    fn peek_is_union(&self) -> bool {
        self.peek_kw("union")
    }

    /// True if the next token is the word `word`, in any case. Words such as
    /// `UNION`, `REMOVE`, `FOREACH`, `EXISTS`, `CALL` and `EXPLAIN` are lexed as
    /// plain identifiers; a parser position where a clause or statement can
    /// start (or `EXISTS {` in an expression) asks `peek_kw` and treats the word
    /// as a keyword only there. Everywhere else it stays a name.
    fn peek_kw(&self, word: &str) -> bool {
        kw(self.peek(), word)
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

    /// The refusal for an openCypher form this release does not run, when the
    /// parse failed at its first token. [`err`](Self::err) asks this first, so
    /// a form that no parser branch accepts is named instead of reported as
    /// an unexpected token. It looks only at a failure point: wherever one of
    /// these words parses (as a name), no error is raised and this never runs.
    fn unsupported_form(&self) -> Option<String> {
        let at = |i: usize| self.toks.get(i);
        let prev = self.pos.checked_sub(1).and_then(at);
        let next = at(self.pos + 1);
        let feature = match self.peek()? {
            Tok::Ident(word) => match word.to_ascii_lowercase().as_str() {
                "call" if matches!(next, Some(Tok::LBrace | Tok::LParen)) => "CALL subquery",
                "call" if matches!(next, Some(Tok::Ident(_))) && self.pos == 0 => {
                    "CALL here (a procedure call runs only as a read query of its own)"
                }
                "call" if matches!(next, Some(Tok::Ident(_))) => "CALL inside a larger query",
                "foreach" if next == Some(&Tok::LParen) => "FOREACH",
                "load" if kw(next, "csv") => "LOAD CSV",
                "use" if self.pos == 0 && matches!(next, Some(Tok::Ident(_))) => {
                    "USE (database selection)"
                }
                "allshortestpaths" if next == Some(&Tok::LParen) => "allShortestPaths",
                _ => return None,
            },
            Tok::LBrace if kw(prev, "exists") => "EXISTS { } subquery",
            Tok::LBrace if kw(prev, "count") => "COUNT { } subquery",
            Tok::LBrace if kw(prev, "collect") => "COLLECT { } subquery",
            Tok::LBrace if next == Some(&Tok::Dot) => "map projection (n {.key, …})",
            Tok::Colon | Tok::Pipe
                if matches!(prev, Some(Tok::Ident(_)))
                    && self.pos.checked_sub(2).and_then(at) == Some(&Tok::Colon) =>
            {
                "more than one label (n:A:B, n:A|B); a node has one label"
            }
            _ => return None,
        };
        Some(refusal(feature))
    }

    /// Whether the tokens at the cursor open a relationship pattern used as an
    /// expression: a node pattern `(` [var] [`:Label`…] [`{…}`] `)` followed by
    /// the start of a relationship (`--`, `-[`, `<-`). No arithmetic or
    /// comparison reads that way: unary minus applies only to a number, so
    /// `(x) - -(…)` and `(x) < -(…)` are parse errors already.
    fn pattern_ahead(&self) -> bool {
        let at = |i: usize| self.toks.get(i);
        let mut i = self.pos;
        if at(i) != Some(&Tok::LParen) {
            return false;
        }
        i += 1;
        if matches!(at(i), Some(Tok::Ident(_))) {
            i += 1;
        }
        while at(i) == Some(&Tok::Colon) && matches!(at(i + 1), Some(Tok::Ident(_))) {
            i += 2;
        }
        if at(i) == Some(&Tok::LBrace) {
            let mut depth = 0usize;
            loop {
                match at(i) {
                    Some(Tok::LBrace) => depth += 1,
                    Some(Tok::RBrace) => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    None => return false,
                    _ => {}
                }
                i += 1;
            }
            i += 1;
        }
        if at(i) != Some(&Tok::RParen) {
            return false;
        }
        match (at(i + 1), at(i + 2), at(i + 3)) {
            (Some(Tok::Dash), Some(Tok::Dash), Some(Tok::LParen | Tok::Gt)) => true,
            (Some(Tok::Dash), Some(Tok::LBracket), after) => matches!(
                (after, at(i + 4)),
                (Some(Tok::Colon | Tok::RBracket | Tok::Star), _)
                    | (
                        Some(Tok::Ident(_)),
                        Some(Tok::Colon | Tok::RBracket | Tok::Star | Tok::LBrace)
                    )
            ),
            (Some(Tok::Lt), Some(Tok::Dash), Some(Tok::LBracket | Tok::Dash)) => true,
            _ => false,
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
        let patterns = vec![self.match_pattern()?];
        // Optional WHERE inside the optional scope.
        let where_expr = if self.eat(&Tok::Where) {
            Some(self.where_expr()?)
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
    /// `scope` is what is in scope before the WITH (for `WITH *`).
    fn with_stage(&mut self, scope: &Scope) -> Result<WithStage, String> {
        self.expect(&Tok::With, "expected WITH")?;
        let items = self.projection_items(scope)?;
        // Optional WHERE / HAVING immediately after WITH items.
        let where_expr = if self.eat(&Tok::Where) {
            Some(self.where_expr()?)
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
        let (matches, optional_clauses, stage_unwinds, post_where) = self.with_stage_tail()?;
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

    /// The clauses that may follow a WITH (or an UNWIND): `MATCH`es, then
    /// `OPTIONAL MATCH`es, then `UNWIND`s, then one `WHERE` over all of them.
    #[allow(clippy::type_complexity)]
    fn with_stage_tail(
        &mut self,
    ) -> Result<
        (
            Vec<Pattern>,
            Vec<OptionalClause>,
            Vec<UnwindClause>,
            Option<Expr>,
        ),
        String,
    > {
        // Optional MATCH clauses that follow this WITH.
        let mut matches = Vec::new();
        while self.peek() == Some(&Tok::Match) {
            matches.extend(self.match_clause()?);
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
            Some(self.where_expr()?)
        } else {
            None
        };
        Ok((matches, optional_clauses, stage_unwinds, post_where))
    }

    /// Parse `UNWIND <expr> AS <alias>`.
    fn unwind_clause(&mut self) -> Result<UnwindClause, String> {
        self.expect(&Tok::Unwind, "expected UNWIND")?;
        let list = self.unwind_expr()?;
        self.expect(&Tok::As, "expected AS after UNWIND expression")?;
        let alias = self.binding("expected alias identifier after AS")?;
        Ok(UnwindClause { list, alias })
    }

    /// Parse the list expression in an UNWIND clause: any expression that
    /// evaluates to a list, such as `[e1, e2, …]`, `range(1, 5)`, `$items`,
    /// `a + b`, `var.field` or a bare `var`. The plain forms keep their
    /// dedicated variants; everything else becomes `UnwindExpr::Expr`.
    fn unwind_expr(&mut self) -> Result<UnwindExpr, String> {
        if let Some(Tok::Ident(n)) = self.peek() {
            if is_null_literal(n) {
                self.pos += 1;
                return Ok(UnwindExpr::Expr(Operand::Lit(Value::Null)));
            }
        }
        match self.value_expr()? {
            Operand::Lit(Value::List(vals)) => Ok(UnwindExpr::Lit(vals)),
            Operand::Prop { var, field } => Ok(UnwindExpr::Prop { var, field }),
            Operand::Var(name) => Ok(UnwindExpr::Var(name)),
            other => Ok(UnwindExpr::Expr(other)),
        }
    }

    /// Parse one `MATCH p1 [, p2, …]` clause.
    ///
    /// Comma-separated patterns in a single MATCH are the openCypher way to
    /// write an intersection — `MATCH (t)-[:A]->(c), (t)-[:B]->(c)` binds the
    /// *same* `t` and `c` across both patterns — and every multihop question
    /// an assistant asks is that shape. The returned patterns are appended to
    /// the same `Vec<Pattern>` a run of separate MATCH clauses produces; the
    /// second and later parts carry `continues_match`, because the parts of
    /// one clause share relationship uniqueness and separate clauses do not.
    fn match_clause(&mut self) -> Result<Vec<Pattern>, String> {
        self.expect(&Tok::Match, "expected MATCH")?;
        let mut out = vec![self.match_pattern()?];
        while self.eat(&Tok::Comma) {
            let mut part = self.match_pattern()?;
            part.continues_match = true;
            out.push(part);
        }
        Ok(out)
    }

    /// One pattern inside a MATCH clause: `shortestPath(…)` or a plain chain.
    fn match_pattern(&mut self) -> Result<Pattern, String> {
        let path_var = self.path_binding()?;
        // Detect `shortestPath(...)`.
        let mut pat = match self.peek() {
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("shortestpath") => {
                self.shortest_path_clause()?
            }
            _ => self.pattern()?,
        };
        pat.path_var = path_var;
        Ok(pat)
    }

    /// The `p =` that names a path: an identifier followed by `=`. Anything
    /// else leaves the position alone.
    fn path_binding(&mut self) -> Result<Option<String>, String> {
        let is_binding = matches!(self.peek(), Some(Tok::Ident(_)))
            && self.toks.get(self.pos + 1) == Some(&Tok::Eq);
        if !is_binding {
            return Ok(None);
        }
        let name = self.binding("expected a path name before '='")?;
        self.pos += 1; // consume '='
        Ok(Some(name))
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
            path_var: None,
            continues_match: false,
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
            path_var: None,
            continues_match: false,
        })
    }

    fn node(&mut self) -> Result<NodePat, String> {
        self.expect(&Tok::LParen, "expected '(' to start a node pattern")?;
        let var = match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.not_reserved(&s)?;
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
                self.not_reserved(&s)?;
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

    // ── Expression grammar (QL-BX) ─────────────────────────────────────────────
    //
    // One grammar serves every expression position. From loosest to tightest:
    //   or_expr   = xor_expr (OR xor_expr)*
    //   xor_expr  = and_expr (XOR and_expr)*
    //   and_expr  = not_expr (AND not_expr)*
    //   not_expr  = NOT* cmp_expr
    //   cmp_expr  = pred_expr [cmp_op pred_expr]
    //   pred_expr = arith_expr (IS [NOT] NULL | IN in_list
    //                          | STARTS WITH arith | ENDS WITH arith | CONTAINS arith)*
    //   arith_expr … arith_atom, where arith_atom includes `( or_expr )`.
    //
    // A predicate is an `Expr`; in a value position it is carried as
    // `Operand::Pred`. A WHERE takes the same tree back out (`into_predicate`),
    // so its `Expr` is exactly what the WHERE-only grammar built before.

    /// A WHERE (or CASE WHEN, or comprehension WHERE) predicate.
    fn where_expr(&mut self) -> Result<Expr, String> {
        Ok(into_predicate(self.value_expr()?))
    }

    /// A full expression, in any value position.
    fn value_expr(&mut self) -> Result<Operand, String> {
        if self.expr_depth >= MAX_EXPR_DEPTH {
            return Err(self.err("expression nesting too deep"));
        }
        self.expr_depth += 1;
        let out = self.or_expr();
        self.expr_depth -= 1;
        out
    }

    fn or_expr(&mut self) -> Result<Operand, String> {
        let mut left = self.xor_expr()?;
        while self.eat(&Tok::Or) {
            let right = self.xor_expr()?;
            left = pred(Expr::Or(
                Box::new(into_predicate(left)),
                Box::new(into_predicate(right)),
            ));
        }
        Ok(left)
    }

    /// `XOR` is an ordinary identifier to the lexer; after an operand it can
    /// only be the operator.
    fn xor_expr(&mut self) -> Result<Operand, String> {
        let mut left = self.and_expr()?;
        while self.eat_ident_kw("xor") {
            let right = self.and_expr()?;
            left = pred(Expr::Xor(
                Box::new(into_predicate(left)),
                Box::new(into_predicate(right)),
            ));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Operand, String> {
        let mut left = self.not_expr()?;
        while self.eat(&Tok::And) {
            let right = self.not_expr()?;
            left = pred(Expr::And(
                Box::new(into_predicate(left)),
                Box::new(into_predicate(right)),
            ));
        }
        Ok(left)
    }

    /// `NOT` binds looser than a comparison: `NOT a = b` is `NOT (a = b)`.
    fn not_expr(&mut self) -> Result<Operand, String> {
        let mut nots = 0usize;
        while self.eat(&Tok::Not) {
            nots += 1;
            if nots > MAX_PAREN_DEPTH {
                return Err(self.err("expression nesting too deep"));
            }
        }
        let mut inner = self.cmp_expr()?;
        for _ in 0..nots {
            inner = pred(Expr::Not(Box::new(into_predicate(inner))));
        }
        Ok(inner)
    }

    fn cmp_expr(&mut self) -> Result<Operand, String> {
        let lhs = self.pred_expr()?;
        let Some(op) = self.peek_cmp_op() else {
            return Ok(lhs);
        };
        self.pos += 1;
        let rhs = self.pred_expr()?;
        Ok(pred(Expr::Cmp { lhs, op, rhs }))
    }

    /// The postfix predicates: `IS [NOT] NULL`, `IN`, and the infix string
    /// predicates, which bind tighter than a comparison.
    fn pred_expr(&mut self) -> Result<Operand, String> {
        let mut lhs = self.arith_expr()?;
        loop {
            if self.eat_ident_kw("is") {
                let negated = self.eat(&Tok::Not);
                if !self.eat_ident_kw("null") {
                    return Err(self.err(if negated {
                        "expected NULL after IS NOT"
                    } else {
                        "expected NULL after IS"
                    }));
                }
                lhs = pred(if negated {
                    Expr::IsNotNull(lhs)
                } else {
                    Expr::IsNull(lhs)
                });
            } else if self.eat_ident_kw("in") {
                // `IN [a, b, $p]` or `IN $list`.
                let list = self.in_list()?;
                lhs = pred(Expr::In { expr: lhs, list });
            } else if let Some(name) = self.eat_infix_string_op()? {
                // `STARTS WITH`, `ENDS WITH`, `CONTAINS` desugar to the
                // scalar functions of the same name, so null and non-string
                // handling is shared with the `startsWith(a, b)` spelling.
                let rhs = self.arith_expr()?;
                lhs = Operand::FuncCall {
                    name: name.to_string(),
                    args: vec![lhs, rhs],
                };
            } else {
                return Ok(lhs);
            }
        }
    }

    /// Consume `STARTS WITH` / `ENDS WITH` / `CONTAINS` if one follows, and
    /// return the scalar function it desugars to.
    ///
    /// `STARTS` and `ENDS` are ordinary identifiers to the lexer, so a bare
    /// `STARTS` not followed by `WITH` is a named error rather than a silent
    /// reinterpretation of the token as a variable.
    fn eat_infix_string_op(&mut self) -> Result<Option<&'static str>, String> {
        let Some(Tok::Ident(s)) = self.peek() else {
            return Ok(None);
        };
        let word = s.to_ascii_lowercase();
        match word.as_str() {
            "contains" => {
                self.pos += 1;
                Ok(Some("contains"))
            }
            "starts" | "ends" => {
                self.pos += 1;
                // `WITH` is a lexer keyword token here, not an identifier.
                if !self.eat(&Tok::With) {
                    let kw = if word == "starts" { "STARTS" } else { "ENDS" };
                    return Err(self.err(&format!("expected WITH after {kw}")));
                }
                Ok(Some(if word == "starts" {
                    "startsWith"
                } else {
                    "endsWith"
                }))
            }
            _ => Ok(None),
        }
    }

    /// The right-hand side of `IN`: one additive expression, as in openCypher,
    /// so `IN [a, b]`, `IN $list` and `IN [1] + [2]` all name one list.
    fn in_list(&mut self) -> Result<Operand, String> {
        self.arith_expr()
    }

    // ── Arithmetic expression parsing (precedence: ^ > * / % > + -) ──────────────
    //
    // Grammar:
    //   arith_expr  = arith_add
    //   arith_add   = arith_mul ((+ | -) arith_mul)*
    //   arith_mul   = arith_pow ((* | / | %) arith_pow)*
    //   arith_pow   = arith_unary (^ arith_unary)*
    //   arith_unary = - arith_atom | arith_atom
    //   arith_atom  = literal | param | ident | ident.field | ident(args…) | ( value_expr )

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
        let mut left = self.arith_pow()?;
        loop {
            let op = if self.eat(&Tok::Star) {
                ArithOp::Mul
            } else if self.eat(&Tok::Slash) {
                ArithOp::Div
            } else if self.eat(&Tok::Percent) {
                ArithOp::Mod
            } else {
                break;
            };
            let right = self.arith_pow()?;
            left = Operand::BinArith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Parse an exponentiation-level expression: `^` binds tighter than `*`,
    /// is left-associative (`2^3^2` is `(2^3)^2`), and takes unary-minus
    /// operands (`-2^2` is `(-2)^2`).
    fn arith_pow(&mut self) -> Result<Operand, String> {
        let mut left = self.arith_unary()?;
        while self.eat(&Tok::Caret) {
            let right = self.arith_unary()?;
            left = Operand::BinArith {
                op: ArithOp::Pow,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Parse a unary-level arithmetic expression (handles unary `-`).
    fn arith_unary(&mut self) -> Result<Operand, String> {
        if self.eat(&Tok::Dash) {
            // Unary minus: fold into the next atom. Only a number takes one,
            // which `pattern_ahead` relies on: it reads `(v)--(…)` and
            // `(v)<--…` as relationship patterns. A change that lets unary
            // minus take any expression must revisit that lookahead.
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
        // `(n)-[:K]->()` as a value or a predicate.
        if self.pattern_ahead() {
            return Err(refusal(
                "pattern predicate (a relationship pattern used as an expression)",
            ));
        }
        // A parenthesized expression: arithmetic or a predicate.
        if self.eat(&Tok::LParen) {
            // `value_expr` bounds the nesting.
            let inner = self.value_expr()?;
            self.expect(
                &Tok::RParen,
                "expected ')' to close parenthesized expression",
            )?;
            return Ok(inner);
        }
        let atom = match self.peek() {
            Some(Tok::LBracket) => self.list_literal()?,
            Some(Tok::LBrace) => self.map_literal()?,
            // The existing atom parser (no unary minus here — handled above).
            _ => self.operand_atom()?,
        };
        self.subscripts(atom)
    }

    /// Enter one level of literal nesting, or fail before the stack does.
    fn enter_literal(&mut self) -> Result<(), String> {
        if self.literal_depth >= MAX_PAREN_DEPTH {
            return Err(self.err("list or map literal nesting too deep"));
        }
        self.literal_depth += 1;
        Ok(())
    }

    /// `[]` or `[e1, e2, …]`; each element is any expression. A list whose
    /// elements are all constants folds to one `Lit(Value::List)`. A trailing
    /// comma is refused.
    fn list_literal(&mut self) -> Result<Operand, String> {
        self.expect(&Tok::LBracket, "expected '['")?;
        if self.pattern_ahead() {
            return Err(refusal("pattern comprehension [(n)-->(m) | …]"));
        }
        self.enter_literal()?;
        if let (Some(Tok::Ident(_)), Some(Tok::Ident(kw))) =
            (self.peek(), self.toks.get(self.pos + 1))
        {
            if kw.eq_ignore_ascii_case("in") {
                return self.list_comprehension();
            }
        }
        let mut items = Vec::new();
        if !self.eat(&Tok::RBracket) {
            loop {
                items.push(self.value_expr()?);
                if self.eat(&Tok::Comma) {
                    if self.peek() == Some(&Tok::RBracket) {
                        return Err(self.err("trailing comma in a list literal is not allowed"));
                    }
                    continue;
                }
                self.expect(&Tok::RBracket, "expected ',' or ']' in a list literal")?;
                break;
            }
        }
        self.literal_depth -= 1;
        if items.iter().all(|o| matches!(o, Operand::Lit(_))) {
            let values = items
                .into_iter()
                .filter_map(|o| match o {
                    Operand::Lit(v) => Some(v),
                    _ => None,
                })
                .collect();
            return Ok(Operand::Lit(Value::List(values)));
        }
        Ok(Operand::ListLit(items))
    }

    /// The rest of `[x IN list [WHERE pred] [| expr]]`, after the `[` and the
    /// literal-depth entry. A comprehension nests like a list literal, so it
    /// shares `literal_depth` and its limit.
    fn list_comprehension(&mut self) -> Result<Operand, String> {
        let var = self.ident("expected a comprehension variable")?;
        self.eat_ident_kw("in");
        let list = self.value_expr()?;
        let filter = if self.eat(&Tok::Where) {
            Some(Box::new(self.where_expr()?))
        } else {
            None
        };
        let map = if self.eat(&Tok::Pipe) {
            self.value_expr()?
        } else {
            Operand::Var(var.clone())
        };
        self.expect(&Tok::RBracket, "expected ']' to close a list comprehension")?;
        self.literal_depth -= 1;
        Ok(Operand::ListComprehension {
            var,
            list: Box::new(list),
            filter,
            map: Box::new(map),
        })
    }

    /// Whether the tokens after `name` `(` read `ident IN`: the head of a
    /// list predicate. Anything else is an ordinary function call, so a
    /// user function named `any` keeps working.
    fn list_predicate_ahead(&self) -> bool {
        matches!(
            (self.toks.get(self.pos + 1), self.toks.get(self.pos + 2)),
            (Some(Tok::Ident(_)), Some(Tok::Ident(kw))) if kw.eq_ignore_ascii_case("in")
        )
    }

    /// The rest of `all|any|none|single(x IN list WHERE pred)`, after the
    /// name. It nests like a list literal, so it shares `literal_depth` and
    /// its limit.
    fn list_predicate(&mut self, kind: ListPredKind) -> Result<Operand, String> {
        const SHAPE: &str = "a list predicate takes (x IN list WHERE predicate)";
        self.expect(&Tok::LParen, "expected '(' after the list predicate name")?;
        self.enter_literal()?;
        let var = self.ident(SHAPE)?;
        if !self.eat_ident_kw("in") {
            return Err(self.err(SHAPE));
        }
        let list = self.value_expr()?;
        if !self.eat(&Tok::Where) {
            return Err(self.err(SHAPE));
        }
        let pred = self.where_expr()?;
        self.expect(&Tok::RParen, "expected ')' to close a list predicate")?;
        self.literal_depth -= 1;
        Ok(Operand::ListPredicate {
            kind,
            var,
            list: Box::new(list),
            pred: Box::new(pred),
        })
    }

    /// The rest of `reduce(acc = init, x IN list | body)`, after the name.
    /// It nests like a list literal, so it shares `literal_depth` and its
    /// limit.
    fn reduce_call(&mut self) -> Result<Operand, String> {
        const SHAPE: &str = "reduce() takes (acc = init, x IN list | expression)";
        self.expect(&Tok::LParen, "expected '(' after reduce")?;
        self.enter_literal()?;
        let acc_var = self.ident(SHAPE)?;
        self.expect(&Tok::Eq, SHAPE)?;
        let init = self.value_expr()?;
        self.expect(&Tok::Comma, SHAPE)?;
        let iter_var = self.ident(SHAPE)?;
        if !self.eat_ident_kw("in") {
            return Err(self.err(SHAPE));
        }
        if acc_var == iter_var {
            return Err(self.err("reduce() accumulator and element variables must differ"));
        }
        let list = self.value_expr()?;
        self.expect(&Tok::Pipe, SHAPE)?;
        let body = self.value_expr()?;
        self.expect(&Tok::RParen, "expected ')' to close reduce()")?;
        self.literal_depth -= 1;
        Ok(Operand::Reduce {
            acc_var,
            init: Box::new(init),
            iter_var,
            list: Box::new(list),
            body: Box::new(body),
        })
    }

    /// `{}` or `{k1: e1, k2: e2}`; keys are identifiers (backtick-quoted
    /// allowed), values are any expression. A duplicate key is refused (some
    /// engines keep the last one; silently dropping a value hides a typo). An
    /// all-constant map folds to one `Lit(Value::Map)`.
    fn map_literal(&mut self) -> Result<Operand, String> {
        self.expect(&Tok::LBrace, "expected '{'")?;
        self.enter_literal()?;
        let mut entries: Vec<(String, Operand)> = Vec::new();
        if !self.eat(&Tok::RBrace) {
            loop {
                let key = self.ident("expected a map key")?;
                if entries.iter().any(|(k, _)| *k == key) {
                    return Err(self.err(&format!("duplicate map key `{key}` in a map literal")));
                }
                self.expect(&Tok::Colon, "expected ':' after a map key")?;
                let value = self.value_expr()?;
                entries.push((key, value));
                if self.eat(&Tok::Comma) {
                    if self.peek() == Some(&Tok::RBrace) {
                        return Err(self.err("trailing comma in a map literal is not allowed"));
                    }
                    continue;
                }
                self.expect(&Tok::RBrace, "expected ',' or '}' in a map literal")?;
                break;
            }
        }
        self.literal_depth -= 1;
        if entries.iter().all(|(_, o)| matches!(o, Operand::Lit(_))) {
            let map = entries
                .into_iter()
                .filter_map(|(k, o)| match o {
                    Operand::Lit(v) => Some((k, v)),
                    _ => None,
                })
                .collect();
            return Ok(Operand::Lit(Value::Map(map)));
        }
        Ok(Operand::MapLit(entries))
    }

    /// Apply any postfix `[index]` subscripts to an already-parsed operand.
    ///
    /// `n.location[0]` is how an assistant reads one end of a `[lat, lon]`
    /// pair, and it used to stop the parse dead at the `[`. Chained
    /// subscripts (`n.grid[0][1]`) parse; only the first level is ever
    /// non-null for the list shapes the graph stores.
    fn subscripts(&mut self, mut base: Operand) -> Result<Operand, String> {
        while self.eat(&Tok::LBracket) {
            let index = self.value_expr()?;
            self.expect(&Tok::RBracket, "expected ']' to close a list subscript")?;
            base = Operand::Index {
                base: Box::new(base),
                index: Box::new(index),
            };
        }
        Ok(base)
    }

    /// The comparison operator at the cursor, if one is there.
    fn peek_cmp_op(&self) -> Option<CmpOp> {
        Some(match self.peek()? {
            Tok::Eq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Le => CmpOp::Le,
            Tok::Gt => CmpOp::Gt,
            Tok::Ge => CmpOp::Ge,
            _ => return None,
        })
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
                // `$p.field` (chained: `$p.a.b`): the map entry, as `$p['field']`.
                let mut operand = Operand::Param(s);
                while self.peek() == Some(&Tok::Dot)
                    && matches!(self.toks.get(self.pos + 1), Some(Tok::Ident(_)))
                {
                    self.pos += 1;
                    let field = self.ident("expected a key after '.'")?;
                    operand = Operand::Index {
                        base: Box::new(operand),
                        index: Box::new(Operand::Lit(Value::Str(field))),
                    };
                }
                Ok(operand)
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
                if name.eq_ignore_ascii_case("reduce") && self.peek() == Some(&Tok::LParen) {
                    return self.reduce_call();
                }
                if let Some(kind) = list_pred_kind(&name) {
                    if self.peek() == Some(&Tok::LParen) && self.list_predicate_ahead() {
                        return self.list_predicate(kind);
                    }
                }
                if self.peek() == Some(&Tok::LParen) {
                    // Scalar function call: name(arg, ...)
                    let args = self.function_args()?;
                    Ok(Operand::FuncCall { name, args })
                } else if self.eat(&Tok::Dot) {
                    let field = self.ident("expected field name after '.'")?;
                    // `ns.name(…)` is a namespaced function call
                    // (`date.truncate(…)`); no property access is followed
                    // by `(`.
                    if self.peek() == Some(&Tok::LParen) {
                        let args = self.function_args()?;
                        return Ok(Operand::FuncCall {
                            name: format!("{name}.{field}"),
                            args,
                        });
                    }
                    Ok(Operand::Prop { var: name, field })
                } else if let Some(b) = bool_literal(&name) {
                    Ok(Operand::Lit(Value::Bool(b)))
                } else if is_null_literal(&name) {
                    // Evaluates to query null; inside a list or map it is kept
                    // as a `Value::Null` element.
                    Ok(Operand::Lit(Value::Null))
                } else if self.peek() == Some(&Tok::Colon)
                    && matches!(self.toks.get(self.pos + 1), Some(Tok::Ident(_)))
                {
                    self.label_predicate(name)
                } else {
                    // Bare variable reference (e.g. alias name in WITH … WHERE c > 2).
                    Ok(Operand::Var(name))
                }
            }
            _ => Err(self.err("expected operand (property, literal, or parameter)")),
        }
    }

    /// `(arg, …)` of a function call, from its `(`. Arguments may be
    /// arbitrary arithmetic expressions.
    fn function_args(&mut self) -> Result<Vec<Operand>, String> {
        self.expect(&Tok::LParen, "expected '(' to open function call")?;
        let mut args = Vec::new();
        if self.peek() != Some(&Tok::RParen) {
            args.push(self.value_expr()?);
            while self.eat(&Tok::Comma) {
                args.push(self.value_expr()?);
            }
        }
        self.expect(&Tok::RParen, "expected ')' to close function call")?;
        Ok(args)
    }

    /// `var:Label` after the variable: a label predicate, parsed to the
    /// [`LABEL_TEST_FN`] call. A node has one label in 0.10, so `var:A:B` and
    /// `var:A|B` are refused by name.
    fn label_predicate(&mut self, var: String) -> Result<Operand, String> {
        self.expect(&Tok::Colon, "expected ':' before a label")?;
        let label = self.ident("expected a label after ':'")?;
        if matches!(self.peek(), Some(Tok::Colon | Tok::Pipe)) {
            return Err(refusal(
                "more than one label (n:A:B, n:A|B); a node has one label",
            ));
        }
        Ok(Operand::FuncCall {
            name: LABEL_TEST_FN.to_string(),
            args: vec![Operand::Var(var), Operand::Lit(Value::Str(label))],
        })
    }

    /// Parse a generic `CASE WHEN <cond> THEN <value> … [ELSE <value>] END`.
    /// The `CASE` keyword has already been consumed. `WHEN`/`THEN`/`ELSE`/`END`
    /// are ordinary identifiers to the lexer, matched case-insensitively.
    fn parse_case(&mut self) -> Result<Operand, String> {
        let is_kw = |tok: Option<&Tok>, kw: &str| matches!(tok, Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw));
        let mut branches = Vec::new();
        while is_kw(self.peek(), "when") {
            self.pos += 1; // consume WHEN
            let cond = self.where_expr()?;
            if !is_kw(self.peek(), "then") {
                return Err(self.err("expected THEN in a CASE branch"));
            }
            self.pos += 1; // consume THEN
            let value = self.value_expr()?;
            branches.push((cond, value));
        }
        if branches.is_empty() {
            return Err(self.err("CASE requires at least one WHEN ... THEN branch"));
        }
        let default = if is_kw(self.peek(), "else") {
            self.pos += 1; // consume ELSE
            Some(Box::new(self.value_expr()?))
        } else {
            None
        };
        if !is_kw(self.peek(), "end") {
            return Err(self.err("expected END to close CASE"));
        }
        self.pos += 1; // consume END
        Ok(Operand::Case { branches, default })
    }

    /// A MATCH pattern property value: a full expression, like every other
    /// value position.
    fn operand(&mut self) -> Result<Operand, String> {
        self.value_expr()
    }

    /// `RETURN [DISTINCT] items`; `scope` is what is in scope (for `RETURN *`).
    fn return_clause(&mut self, scope: &Scope) -> Result<(bool, Vec<RetItem>), String> {
        if !self.eat(&Tok::Return) {
            return Err(self.unsupported_or_unexpected("expected RETURN"));
        }
        let distinct = self.eat_ident_kw("distinct");
        Ok((distinct, self.projection_items(scope)?))
    }

    /// The items of a RETURN or WITH: `item [, item …]`, or `* [, item …]`,
    /// where `*` stands for every name in `scope` (see [`Scope::star_items`]).
    fn projection_items(&mut self, scope: &Scope) -> Result<Vec<RetItem>, String> {
        let mut items = if self.eat(&Tok::Star) {
            scope.star_items()?
        } else {
            vec![self.ret_item()?]
        };
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
                "stdev" => Some(AggFunc::StDev),
                "stdevp" => Some(AggFunc::StDevP),
                // `p` is filled in after the first argument is parsed.
                "percentilecont" => Some(AggFunc::PercentileCont {
                    p: Operand::Lit(Value::Null),
                }),
                "percentiledisc" => Some(AggFunc::PercentileDisc {
                    p: Operand::Lit(Value::Null),
                }),
                _ => None,
            };
            if let Some(mut func) = func {
                self.pos += 1; // consume the function name
                self.expect(&Tok::LParen, "expected '(' after aggregate function name")?;
                // `count(DISTINCT t)` — DISTINCT is an ordinary identifier to
                // the lexer, so it is only a modifier here when something
                // follows it inside the parens.
                let distinct = matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("distinct"))
                    && !matches!(self.toks.get(self.pos + 1), Some(Tok::RParen));
                if distinct {
                    self.pos += 1;
                    if self.peek() == Some(&Tok::Star) {
                        return Err(self.err("DISTINCT * is not a valid aggregate argument"));
                    }
                }
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
                let arg = if distinct {
                    AggArg::Distinct(Box::new(arg))
                } else {
                    arg
                };
                if let AggFunc::PercentileCont { p } | AggFunc::PercentileDisc { p } = &mut func {
                    if !self.eat(&Tok::Comma) {
                        return Err(
                            self.err("expected ',' and a percentile `p` after the argument")
                        );
                    }
                    *p = self.value_expr()?;
                }
                self.expect(&Tok::RParen, "expected ')' to close aggregate function")?;
                let alias = if self.eat(&Tok::As) {
                    Some(self.binding("expected alias identifier after AS")?)
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
        let op = self.value_expr()?;
        let value = match op {
            Operand::Var(name) => RetVal::Var(name),
            Operand::Prop { var, field } => RetVal::Prop { var, field },
            // A label test names its column like any other predicate.
            Operand::FuncCall { name, args } if name != LABEL_TEST_FN => {
                RetVal::FuncCall { name, args }
            }
            other => RetVal::ScalarExpr(other),
        };
        let alias = if self.eat(&Tok::As) {
            Some(self.binding("expected alias identifier after AS")?)
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
        let target = match self.value_expr()? {
            Operand::Prop { var, field } => OrderTarget::Prop { var, field },
            Operand::Var(name) if aliases.contains(&name.as_str()) => OrderTarget::Alias(name),
            Operand::Var(name) => OrderTarget::Var(name),
            other => OrderTarget::Expr(other),
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
            Some(Tok::Create) | Some(Tok::Merge)
                if needs_per_row_form(&self.toks[self.pos..])
                    || writes_relationship_properties(&self.toks[self.pos..]) =>
            {
                self.unwind_write_stmt()
            }
            Some(Tok::Create) => self.create_stmt(),
            Some(Tok::Merge) => self.merge_stmt(),
            Some(Tok::Match) => self.match_write_stmt(),
            Some(Tok::Unwind) => self.unwind_write_stmt(),
            _ => Err(self.err(
                "expected CREATE, MERGE, MATCH … SET/DELETE, or UNWIND … CREATE/MERGE/SET \
                 (write statement required)",
            )),
        }
    }

    // ── CREATE ────────────────────────────────────────────────────────────────

    fn create_stmt(&mut self) -> Result<WriteStatement, String> {
        self.expect(&Tok::Create, "expected CREATE")?;
        let mut stmt = self.create_pattern()?;
        // Optional RETURN clause: `CREATE (n:L {…}) RETURN n` or `RETURN n.id AS id`.
        if self.eat(&Tok::Return) {
            let mut scope = Scope::default();
            for (i, node) in stmt.nodes.iter().enumerate() {
                match &node.var {
                    // `create_node` names an anonymous node `_cn<index>`.
                    Some(v) if *v != format!("_cn{i}") => scope.bind(v, VarKind::Node),
                    _ => {}
                }
            }
            stmt.returns = Some(self.projection_items(&scope)?);
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
                rel_var: None,
                props: Vec::new(),
                props_map: None,
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
                self.not_reserved(&s)?;
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

    /// Parse the `{key: value, …}` of a plain CREATE or MERGE. A value is a
    /// literal, or any expression that reads no variable, property or
    /// `$parameter` (`1 > 0`, `'a' + 'b'`), which is evaluated here so the
    /// write (and a MERGE's match) sees the value. A statement whose map reads
    /// a `$parameter` is the per-row form instead (see [`needs_per_row_form`]).
    fn literal_props(&mut self) -> Result<Vec<(String, Value)>, String> {
        self.expect(&Tok::LBrace, "expected '{'")?;
        let mut out = Vec::new();
        if self.eat(&Tok::RBrace) {
            return Ok(out);
        }
        loop {
            let key = self.ident("expected property key")?;
            self.expect(&Tok::Colon, "expected ':' after property key")?;
            let val = self.property_value(&key)?;
            out.push((key, val));
            if self.eat(&Tok::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Tok::RBrace, "expected '}' to close property map")?;
        Ok(out)
    }

    /// One value of [`literal_props`](Self::literal_props). A plain literal
    /// keeps its own parse (and its named errors: a `null` value, a `$param`,
    /// a variable); anything else is a constant expression.
    fn property_value(&mut self, key: &str) -> Result<Value, String> {
        const WHAT: &str = "property value";
        let start = self.pos;
        let literal = self.literal_value(WHAT);
        if let Ok(value) = &literal {
            if matches!(self.peek(), Some(Tok::Comma) | Some(Tok::RBrace)) {
                return Ok(value.clone());
            }
        }
        let literal_err = literal.err();
        self.pos = start;
        let operand = match self.value_expr() {
            Ok(Operand::Lit(Value::Null)) => {
                self.pos = start;
                return Err(self.err(&format!(
                    "property `{key}` is null (a CREATE or MERGE property value cannot be null)"
                )));
            }
            Err(_) => {
                self.pos = start;
                return Err(literal_err.unwrap_or_else(|| self.err("expected a property value")));
            }
            Ok(operand) => operand,
        };
        match super::exec::fold_constant(&operand) {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(self.err(&format!(
                "property `{key}` evaluated to null (a CREATE or MERGE property value cannot be null)"
            ))),
            // Unreachable from `write_statement`, which gives a statement
            // whose map reads a parameter the per-row form.
            Err(super::exec::FoldError::Param) => Err(self.err(&format!(
                "property `{key}` reads a $parameter, which this form of the statement cannot evaluate"
            ))),
            Err(super::exec::FoldError::Variable) => Err(self.err(&format!(
                "property `{key}` reads a variable, and no clause before this CREATE or MERGE binds one"
            ))),
            Err(super::exec::FoldError::Eval(e)) => Err(format!("property `{key}`: {e}")),
        }
    }

    /// Parse a literal value (int, float, string, boolean or list). Anything
    /// else (a parameter, a variable, an expression) is an error, which
    /// [`property_value`](Self::property_value) answers by parsing an
    /// expression instead.
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
                        // A null element parses so the write path can refuse
                        // it by name; a top-level `null` keeps the error below.
                        if matches!(self.peek(), Some(Tok::Ident(n)) if is_null_literal(n)) {
                            self.pos += 1;
                            items.push(Value::Null);
                        } else {
                            items.push(self.literal_value(what)?);
                        }
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        self.expect(&Tok::RBracket, "expected ']' to close list literal")?;
                        break;
                    }
                }
                Ok(Value::List(items))
            }
            Some(Tok::Ident(name)) if bool_literal(name).is_some() => {
                let b = bool_literal(name).unwrap_or_default();
                self.pos += 1;
                Ok(Value::Bool(b))
            }
            Some(Tok::Param(_)) => {
                Err(self.err(&format!("expected a literal {what}, not a $parameter")))
            }
            Some(Tok::Ident(_)) => {
                Err(self.err(&format!("expected a literal {what}, not a variable")))
            }
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

    /// One MERGE node pattern `(var:Label {key: 'x' [, ns: 'y']})`.
    fn merge_node(&mut self, open_msg: &str) -> Result<MergeNode, String> {
        self.merge_node_with(open_msg, Self::literal_props)
    }

    /// [`merge_node`](Self::merge_node) with the property values read by
    /// `props`: literals for a plain MERGE, expressions for a per-row one.
    fn merge_node_with<V>(
        &mut self,
        open_msg: &str,
        props: PropsParser<'a, V>,
    ) -> Result<MergeNode<V>, String> {
        self.expect(&Tok::LParen, open_msg)?;
        // Optional var: `MERGE (n:Label {…})` — capture n for RETURN projection.
        let var = match self.peek() {
            Some(Tok::Ident(_)) => {
                let s = match self.toks.get(self.pos) {
                    Some(Tok::Ident(s)) => s.clone(),
                    _ => unreachable!(),
                };
                self.not_reserved(&s)?;
                self.pos += 1; // consume var name
                Some(s)
            }
            _ => None,
        };
        if !self.eat(&Tok::Colon) {
            return Err(self.err("MERGE requires a label (e.g., MERGE (n:Label {key: 'x'}))"));
        }
        let label = self.ident("expected label identifier after ':'")?;
        if matches!(self.peek(), Some(Tok::Param(_))) {
            return Err(refusal(
                "a $parameter map in a MERGE pattern; name the key and set the rest, \
                 e.g. MERGE (n:Label {id: $p.id}) ON CREATE SET n += $p",
            ));
        }
        if self.peek() != Some(&Tok::LBrace) {
            return Err(self.err(
                "MERGE requires a property map with exactly one key (e.g., MERGE (n:Label {id: 'x'}))",
            ));
        }
        let props = props(self)?;
        let mut ns = None;
        let mut rest = Vec::new();
        for (k, v) in props {
            if k == NS_PROP {
                if ns.is_some() {
                    return Err(
                        self.err("ns is given more than once; a node has exactly one namespace")
                    );
                }
                ns = Some(v);
            } else {
                rest.push((k, v));
            }
        }
        if rest.len() != 1 {
            // A lone `ns` is the identifying key, not a namespace stamp.
            if rest.is_empty() {
                if let Some(v) = ns.take() {
                    rest.push((NS_PROP.to_string(), v));
                }
            }
            if rest.len() != 1 {
                return Err(format!(
                    "MERGE supports exactly one key property (got {}); use CREATE for multi-prop nodes",
                    rest.len() + usize::from(ns.is_some()),
                ));
            }
        }
        self.expect(&Tok::RParen, "expected ')' to close MERGE pattern")?;
        let (key_field, key_value) = rest.remove(0);
        Ok(MergeNode {
            var,
            label,
            key_field,
            key_value,
            ns,
        })
    }

    /// The `-[:T]->` / `<-[:T]-` of a MERGE pattern: one typed, directed,
    /// single-hop relationship.
    fn merge_rel(&mut self) -> Result<(Option<String>, String, RelDir), String> {
        let rel = self.rel()?;
        self.merge_rel_checked(rel)
    }

    /// MERGE's checks on its relationship: single-hop, directed, one type.
    fn merge_rel_checked(&self, rel: RelPat) -> Result<(Option<String>, String, RelDir), String> {
        if rel.hops.is_some() {
            return Err(
                "not supported in mushroomdb 0.10: MERGE with a variable-length relationship"
                    .to_string(),
            );
        }
        if rel.dir == RelDir::Undirected {
            return Err(
                "not supported in mushroomdb 0.10: MERGE with an undirected relationship; \
                 give it a direction (-[:T]-> or <-[:T]-)"
                    .to_string(),
            );
        }
        match rel.etypes.as_slice() {
            [one] => Ok((rel.var, one.clone(), rel.dir)),
            [] => Err("MERGE requires a relationship type (e.g., -[:KNOWS]->)".to_string()),
            _ => Err(
                "not supported in mushroomdb 0.10: MERGE with alternative relationship types"
                    .to_string(),
            ),
        }
    }

    fn merge_stmt(&mut self) -> Result<WriteStatement, String> {
        self.expect(&Tok::Merge, "expected MERGE")?;
        let start = self.merge_node("expected '(' after MERGE")?;
        let edge = if matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
            let (rel_var, etype, dir) = self.merge_rel()?;
            let end = self.merge_node("expected '(' to start the MERGE end node")?;
            if matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
                return Err(
                    "not supported in mushroomdb 0.10: MERGE with more than one relationship"
                        .to_string(),
                );
            }
            Some(MergeEdgePat {
                etype,
                dir,
                end_label: end.label,
                end_key_field: end.key_field,
                end_key_value: end.key_value,
                end_ns: end.ns,
                end_var: end.var,
                rel_var,
            })
        } else {
            None
        };
        let MergeNode {
            var,
            label,
            key_field,
            key_value,
            ns,
        } = start;
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        while self.eat_ident_kw("on") {
            if self.eat(&Tok::Create) {
                self.expect(&Tok::Set, "expected SET after ON CREATE")?;
                on_create.extend(self.merge_set_clauses()?);
            } else if self.eat(&Tok::Match) {
                self.expect(&Tok::Set, "expected SET after ON MATCH")?;
                on_match.extend(self.merge_set_clauses()?);
            } else {
                return Err(self.err("expected CREATE or MATCH after ON"));
            }
        }
        // Optional RETURN clause.
        let returns = if self.eat(&Tok::Return) {
            let mut scope = Scope::default();
            let names = [
                (var.as_ref(), VarKind::Node),
                (
                    edge.as_ref().and_then(|e| e.end_var.as_ref()),
                    VarKind::Node,
                ),
                (edge.as_ref().and_then(|e| e.rel_var.as_ref()), VarKind::Rel),
            ];
            for (name, kind) in names {
                if let Some(name) = name {
                    scope.bind(name, kind);
                }
            }
            Some(self.projection_items(&scope)?)
        } else {
            None
        };
        if self.pos < self.toks.len() {
            return Err(self.unsupported_or_unexpected("unexpected tokens after MERGE"));
        }
        Ok(WriteStatement::Merge(MergeStmt {
            label,
            key_field,
            key_value,
            ns,
            var,
            on_create,
            on_match,
            returns,
            edge,
        }))
    }

    // ── MATCH … SET / MATCH … DELETE ─────────────────────────────────────────

    fn match_write_stmt(&mut self) -> Result<WriteStatement, String> {
        // `MATCH … CREATE …` / `MATCH … MERGE …` run per matched row, as the
        // `UNWIND [1] AS _ MATCH …` form does.
        // So do the statements that write a relationship's properties
        // (`MATCH ()-[r]->() SET r.w = 1`): the per-row executor writes them.
        if self.toks[self.pos..]
            .iter()
            .any(|t| matches!(t, Tok::Create | Tok::Merge))
            || writes_relationship_properties(&self.toks[self.pos..])
        {
            return self.unwind_write_stmt();
        }
        // Parse MATCH clauses (same as read query).
        let mut matches = Vec::new();
        while self.peek() == Some(&Tok::Match) {
            matches.extend(self.match_clause()?);
        }
        if matches.is_empty() {
            return Err(self.err("expected MATCH"));
        }
        // Optional WHERE.
        let where_expr = if self.eat(&Tok::Where) {
            Some(self.where_expr()?)
        } else {
            None
        };
        // Dispatch on SET, DETACH DELETE, or DELETE.
        match self.peek() {
            Some(Tok::Set) if self.set_map_form_ahead() => {
                self.pos += 1; // consume SET
                let (var, merge, map, param) = self.set_map_clause(&matches)?;
                let returns = if self.eat(&Tok::Return) {
                    Some(self.projection_items(&Scope::of_patterns(&matches))?)
                } else {
                    None
                };
                if self.pos < self.toks.len() {
                    return Err(self.unsupported_or_unexpected("unexpected tokens after SET"));
                }
                Ok(WriteStatement::MatchSetMap(MatchSetMapStmt {
                    matches,
                    where_expr,
                    merge,
                    var,
                    map,
                    param,
                    returns,
                }))
            }
            Some(Tok::Set) => {
                self.pos += 1; // consume SET
                let sets = self.set_clauses()?;
                let returns = if self.eat(&Tok::Return) {
                    Some(self.projection_items(&Scope::of_patterns(&matches))?)
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
            _ if self.peek_kw("remove") => {
                self.pos += 1; // consume REMOVE
                let removals = self.remove_items(&matches)?;
                let returns = if self.eat(&Tok::Return) {
                    Some(self.projection_items(&Scope::of_patterns(&matches))?)
                } else {
                    None
                };
                if self.pos < self.toks.len() {
                    return Err(self.unsupported_or_unexpected("unexpected tokens after REMOVE"));
                }
                Ok(WriteStatement::MatchRemove(MatchRemoveStmt {
                    matches,
                    where_expr,
                    removals,
                    returns,
                }))
            }
            _ => Err(self.err(
                "expected SET, REMOVE or DELETE after MATCH [WHERE]; \
                 combined MATCH…RETURN is a read query, not a write statement",
            )),
        }
    }

    /// True when the SET at the cursor is a map form: `SET var = …` or
    /// `SET var += …` (a name straight after SET, then `=` or `+ =`, no `.`).
    fn set_map_form_ahead(&self) -> bool {
        matches!(self.toks.get(self.pos + 1), Some(Tok::Ident(_)))
            && matches!(
                (self.toks.get(self.pos + 2), self.toks.get(self.pos + 3)),
                (Some(Tok::Eq), _) | (Some(Tok::Plus), Some(Tok::Eq))
            )
    }

    /// `var = {…}` or `var += {…}` after SET: the variable, whether it is the
    /// merge form, and the map's entries. The variable must be
    /// a node bound by `matches`; the right side must be a map literal or a
    /// `$parameter` (another variable is refused by name); a second SET item
    /// after the map is refused, since the forms do not mix in one clause.
    fn set_map_clause(&mut self, matches: &[Pattern]) -> Result<SetMapClause, String> {
        let var = self.ident("expected variable after SET")?;
        let merge = self.eat(&Tok::Plus);
        self.expect(&Tok::Eq, "expected '=' in SET clause")?;
        let param = match self.peek() {
            Some(Tok::Param(name)) => Some(name.clone()),
            Some(Tok::LBrace) => None,
            _ => {
                return Err(format!(
                    "not supported in mushroomdb 0.10: SET {var} {}= with a right side that is \
                     neither a map literal ({{key: value, …}}) nor a $parameter",
                    if merge { "+" } else { "" }
                ));
            }
        };
        let is_rel = matches
            .iter()
            .flat_map(|p| p.chain.iter())
            .any(|(rel, _)| rel.var.as_deref() == Some(var.as_str()));
        let is_node = matches.iter().any(|p| {
            p.start.var.as_deref() == Some(var.as_str())
                || p.chain
                    .iter()
                    .any(|(_, dest)| dest.var.as_deref() == Some(var.as_str()))
        });
        if is_rel {
            return Err(format!(
                "SET {var} = map: '{var}' is a relationship; the map forms work on nodes"
            ));
        }
        if !is_node {
            return Err(format!(
                "SET {var} = map: '{var}' is not a node variable bound by MATCH"
            ));
        }
        if param.is_some() {
            self.pos += 1;
        }
        let map = match if param.is_some() {
            Operand::MapLit(vec![])
        } else {
            self.map_literal()?
        } {
            Operand::Lit(Value::Map(entries)) => entries
                .into_iter()
                .map(|(k, v)| (k, Operand::Lit(v)))
                .collect(),
            Operand::MapLit(entries) => entries,
            _ => return Err(self.err("expected a map literal after SET")),
        };
        if self.peek() == Some(&Tok::Comma) {
            return Err(
                "not supported in mushroomdb 0.10: a SET map form followed by another SET item; \
                 use one SET clause per statement"
                    .to_string(),
            );
        }
        Ok((var, merge, map, param))
    }

    /// `var.field [, var.field …]` after REMOVE. Each `var` must be a node bound
    /// by `matches`. A label removal (`n:Label`) is refused by name: a node has
    /// one label in 0.10.
    fn remove_items(&mut self, matches: &[Pattern]) -> Result<Vec<(String, String)>, String> {
        let mut items = Vec::new();
        loop {
            let var = self.ident("expected variable after REMOVE")?;
            if self.peek() == Some(&Tok::Colon) {
                return Err(
                    "not supported in mushroomdb 0.10: REMOVE n:Label (label removal)".to_string(),
                );
            }
            self.expect(&Tok::Dot, "expected '.' after variable in REMOVE")?;
            let field = self.ident("expected property name after '.' in REMOVE")?;
            let is_rel = matches
                .iter()
                .flat_map(|p| p.chain.iter())
                .any(|(rel, _)| rel.var.as_deref() == Some(var.as_str()));
            if is_rel {
                return Err(format!(
                    "REMOVE {var}.{field}: '{var}' is a relationship; REMOVE works on node properties"
                ));
            }
            let is_node = matches.iter().any(|p| {
                p.start.var.as_deref() == Some(var.as_str())
                    || p.chain
                        .iter()
                        .any(|(_, dest)| dest.var.as_deref() == Some(var.as_str()))
            });
            if !is_node {
                return Err(format!(
                    "REMOVE {var}.{field}: '{var}' is not a node variable bound by MATCH"
                ));
            }
            items.push((var, field));
            if !self.eat(&Tok::Comma) {
                return Ok(items);
            }
        }
    }

    /// `set_clauses` for a plain `MERGE … ON CREATE/ON MATCH SET`. A map form
    /// (`n = {…}`, `n += $p`) never reaches here: `write_statement` gives such
    /// a statement the per-row form ([`needs_per_row_form`]).
    fn merge_set_clauses(&mut self) -> Result<Vec<SetClause>, String> {
        self.set_clauses()
    }

    fn set_clauses(&mut self) -> Result<Vec<SetClause>, String> {
        let mut sets = vec![self.set_clause()?];
        while self.eat(&Tok::Comma) {
            sets.push(self.set_clause()?);
        }
        Ok(sets)
    }

    fn set_clause(&mut self) -> Result<SetClause, String> {
        self.set_clause_with(false)
    }

    /// One `var.field = expr`. `row` is the per-row form (after UNWIND), where a
    /// bare property or variable is a value like any other.
    fn set_clause_with(&mut self, row: bool) -> Result<SetClause, String> {
        let var = self.ident("expected variable in SET clause")?;
        if self.peek() == Some(&Tok::Colon) {
            return Err(refusal("SET n:Label (label assignment)"));
        }
        self.expect(&Tok::Dot, "expected '.' after variable in SET")?;
        let field = self.ident("expected field name after '.'")?;
        self.expect(&Tok::Eq, "expected '=' in SET clause")?;
        // Accept Lit, Param, BinArith (arithmetic expression), FuncCall and a
        // predicate (`n.score > 10`) on the RHS. Bare Prop/Var (e.g. `SET n.x = m.y`) remain a named error
        // because that form requires join semantics not supported in v1; use
        // an arithmetic expression like `m.y + 0` if needed.
        let value = match self.peek() {
            Some(
                Tok::Int(_)
                | Tok::Float(_)
                | Tok::Str(_)
                | Tok::Param(_)
                | Tok::Dash
                | Tok::Ident(_)
                | Tok::LBracket
                | Tok::LBrace
                | Tok::LParen
                | Tok::Not,
            ) => {
                let op = self.value_expr()?;
                match &op {
                    Operand::Pred(_)
                    | Operand::Lit(_)
                    | Operand::Param(_)
                    | Operand::BinArith { .. }
                    | Operand::FuncCall { .. }
                    | Operand::Index { .. }
                    | Operand::ListLit(_)
                    | Operand::MapLit(_)
                    | Operand::ListComprehension { .. }
                    | Operand::Reduce { .. }
                    | Operand::ListPredicate { .. }
                    | Operand::Case { .. } => op,
                    Operand::Prop { .. } | Operand::Var(_) if row => op,
                    Operand::Prop { .. } | Operand::Var(_) => {
                        return Err(self.err(
                            "SET RHS: bare property/variable reference is not supported; \
                             use a literal, $parameter, or arithmetic expression (e.g. n.x + 1)",
                        ));
                    }
                }
            }
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

    /// Parse `RETURN <expr> AS v` and give back the scalar expression.
    fn return_expr(expr: &str) -> Operand {
        use crate::cypher::ast::RetVal;
        let q = parse_src(&format!("MATCH (n) RETURN {expr} AS v")).unwrap();
        match q.returns.into_iter().next().unwrap().value {
            RetVal::ScalarExpr(op) => op,
            other => panic!("expected a scalar expression, got {other:?}"),
        }
    }

    fn bin(op: crate::cypher::ast::ArithOp, l: Operand, r: Operand) -> Operand {
        Operand::BinArith {
            op,
            left: Box::new(l),
            right: Box::new(r),
        }
    }

    fn int(n: i64) -> Operand {
        Operand::Lit(Value::Int(n))
    }

    #[test]
    fn modulo_shares_a_level_with_mul_and_div_left_associative() {
        use crate::cypher::ast::ArithOp::{Div, Mod, Mul};
        assert_eq!(return_expr("7 % 3"), bin(Mod, int(7), int(3)));
        assert_eq!(
            return_expr("10 % 4 * 2"),
            bin(Mul, bin(Mod, int(10), int(4)), int(2))
        );
        assert_eq!(
            return_expr("10 * 4 % 3 / 2"),
            bin(Div, bin(Mod, bin(Mul, int(10), int(4)), int(3)), int(2))
        );
    }

    #[test]
    fn caret_binds_tighter_than_mul_and_is_left_associative() {
        use crate::cypher::ast::ArithOp::{Add, Mul, Pow};
        assert_eq!(return_expr("2 ^ 8"), bin(Pow, int(2), int(8)));
        assert_eq!(
            return_expr("2 ^ 3 ^ 2"),
            bin(Pow, bin(Pow, int(2), int(3)), int(2))
        );
        assert_eq!(
            return_expr("2 * 3 ^ 2"),
            bin(Mul, int(2), bin(Pow, int(3), int(2)))
        );
        assert_eq!(
            return_expr("1 + 2 ^ 3"),
            bin(Add, int(1), bin(Pow, int(2), int(3)))
        );
        // unary minus binds tighter than ^: -2^2 is (-2)^2
        assert_eq!(return_expr("-2 ^ 2"), bin(Pow, int(-2), int(2)));
    }

    #[test]
    fn words_that_became_keywords_still_parse_as_names() {
        for src in [
            "MATCH (call:Remove) WHERE call.exists = 1 RETURN call.explain AS foreach",
            "MATCH (n:Exists {call: 1, Remove: 2})-[r:FOREACH]->(m) RETURN n.Call, r, m",
            "MATCH (n) WITH n AS explain RETURN explain.remove",
        ] {
            parse_src(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        }
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
                path_var: None,
                continues_match: false,
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
                    path_var: None,
                    continues_match: false,
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
                    path_var: None,
                    continues_match: false,
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
                    path_var: None,
                    continues_match: false,
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
                // One list-valued right-hand side (QL-BX): a written list
                // with a non-constant element stays a list literal.
                assert_eq!(
                    list,
                    Operand::ListLit(vec![
                        Operand::Lit(Value::Str("Austin".into())),
                        Operand::Param("c".into()),
                    ])
                );
            }
            other => panic!("expected Expr::In, got {other:?}"),
        }
        let q2 = parse_src("MATCH (n) WHERE n.city IN $cities RETURN n").unwrap();
        match q2.where_expr {
            Some(Expr::In { list, .. }) => {
                assert_eq!(list, Operand::Param("cities".into()));
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
    fn union_query_parses_into_parts() {
        use super::parse_read;
        let u = parse_read(&lex("MATCH (n) RETURN n UNION ALL MATCH (m) RETURN m").unwrap())
            .expect("UNION parses");
        assert_eq!(u.parts.len(), 2);
        assert_eq!(u.all_flags, vec![true]);
        // A single query yields one part, no boundaries.
        let single = parse_read(&lex("MATCH (n) RETURN n").unwrap()).unwrap();
        assert_eq!(single.parts.len(), 1);
        assert!(single.all_flags.is_empty());
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
    fn merge_ns_property_parses_beside_the_key() {
        use super::parse_write;
        use crate::cypher::ast::WriteStatement;

        let toks = crate::cypher::lex("MERGE (n:Doc {id: 'x', ns: 'other'})").unwrap();
        let stmt = parse_write(&toks).expect("MERGE may name ns beside the identifying key");
        match stmt {
            WriteStatement::Merge(s) => {
                assert_eq!(s.key_field, "id");
                assert_eq!(s.key_value, Value::Str("x".into()));
                assert_eq!(s.ns, Some(Value::Str("other".into())));
            }
            other => panic!("expected Merge, got {other:?}"),
        }

        let toks = crate::cypher::lex("MERGE (n:Doc {id: 'x', name: 'Alice'})").unwrap();
        let err = parse_write(&toks).expect_err("a non-ns extra property is still refused");
        assert!(
            err.contains("one key property") || err.contains("exactly one"),
            "error must mention the single-key constraint, got: {err}"
        );
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

    // ── QL-16: list and map literals ─────────────────────────────────────────

    fn parse_err_of(src: &str) -> String {
        match parse_src(src) {
            Ok(q) => panic!("`{src}` must not parse, got {q:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn ql16_all_literal_lists_and_maps_fold_to_values() {
        assert_eq!(
            return_expr("[1, 'a', [2]]"),
            Operand::Lit(Value::List(vec![
                Value::Int(1),
                Value::Str("a".into()),
                Value::List(vec![Value::Int(2)]),
            ]))
        );
        assert_eq!(return_expr("[]"), Operand::Lit(Value::List(vec![])));
        assert_eq!(
            return_expr("{a: 1, `b c`: 2}"),
            Operand::Lit(Value::Map(
                [
                    ("a".to_string(), Value::Int(1)),
                    ("b c".to_string(), Value::Int(2))
                ]
                .into_iter()
                .collect()
            ))
        );
    }

    #[test]
    fn ql16_non_literal_elements_stay_expressions() {
        assert_eq!(
            return_expr("[n.x, 1]"),
            Operand::ListLit(vec![prop("n", "x"), int(1)])
        );
        assert_eq!(
            return_expr("{k: n.x}"),
            Operand::MapLit(vec![("k".to_string(), prop("n", "x"))])
        );
    }

    #[test]
    fn ql16_literal_errors_are_clear() {
        assert!(parse_err_of("MATCH (n) RETURN [1, 2,] AS x").contains("trailing comma"));
        assert!(parse_err_of("MATCH (n) RETURN {a: 1,} AS x").contains("trailing comma"));
        assert!(
            parse_err_of("MATCH (n) RETURN {a: 1, a: 2} AS x").contains("duplicate map key `a`")
        );
        assert!(parse_err_of("MATCH (n) RETURN [1, 2 AS x").contains("']'"));
        assert!(parse_err_of("MATCH (n) RETURN {a: 1 AS x").contains("'}'"));
        assert!(parse_err_of("MATCH (n) RETURN {a 1} AS x").contains("':'"));
        assert!(parse_err_of("MATCH (n) RETURN [").contains("expected"));
        assert!(parse_err_of("MATCH (n) RETURN {").contains("expected"));
    }

    #[test]
    fn ql16_pattern_maps_and_relationship_brackets_parse_as_before() {
        let q = parse_src("MATCH (a:L {k: 1})-[r:T]->(b) RETURN a").unwrap();
        let pat = &q.matches[0];
        assert_eq!(pat.start.props, vec![("k".to_string(), int(1))]);
        assert_eq!(pat.chain[0].0.etypes, vec!["T".to_string()]);
        // A list-valued property in a pattern now parses as an operand.
        assert!(parse_src("MATCH (a:L {k: [1, 2]}) RETURN a").is_ok());
    }

    #[test]
    fn ql16_deeply_nested_literals_are_an_error_not_a_stack_overflow() {
        let src = format!(
            "MATCH (n) RETURN {}1{} AS x",
            "[".repeat(5000),
            "]".repeat(5000)
        );
        assert!(parse_err_of(&src).contains("too deep"));
    }
}
