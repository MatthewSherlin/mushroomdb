//! Clauses after `UNWIND` (QL-09b): the read clauses that may follow it
//! (`MATCH`, `OPTIONAL MATCH`, `UNWIND`, `WITH`) in both read queries and write
//! statements, and the write statements that begin with `UNWIND`:
//! `UNWIND $rows AS r CREATE …`, `… MERGE …`, `… MATCH … SET …`, and so on.

use super::Parser;
use crate::cypher::ast::{
    CreateEdge, OptionalClause, Pattern, RetItem, RetVal, RowCreateNode, RowMerge, RowMergeEdge,
    RowMergeNode, RowReturn, RowWrite, SetClause, UnwindClause, UnwindWriteStmt, WithStage,
    WriteStatement,
};
use crate::cypher::{Operand, Query, Tok};

/// What a name in scope holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VarKind {
    Node,
    Rel,
    /// A value: an UNWIND alias, a `WITH` alias or any other expression.
    Scalar,
}

/// The names in scope at a point of a read part, with what each holds.
#[derive(Debug, Default)]
pub(super) struct Scope {
    vars: Vec<(String, VarKind)>,
}

impl Scope {
    fn bind(&mut self, name: &str, kind: VarKind) {
        match self.vars.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = kind,
            None => self.vars.push((name.to_string(), kind)),
        }
    }

    fn kind(&self, name: &str) -> Option<VarKind> {
        self.vars.iter().find(|(n, _)| n == name).map(|(_, k)| *k)
    }

    fn bind_pattern(&mut self, pattern: &Pattern) {
        if let Some(var) = &pattern.start.var {
            self.bind(var, VarKind::Node);
        }
        for (rel, dest) in &pattern.chain {
            if let Some(var) = &rel.var {
                self.bind(var, VarKind::Rel);
            }
            if let Some(var) = &dest.var {
                self.bind(var, VarKind::Node);
            }
        }
    }

    /// Bind what a run of read clauses introduces.
    pub(super) fn bind_clauses(
        &mut self,
        matches: &[Pattern],
        optional_clauses: &[OptionalClause],
        unwinds: &[UnwindClause],
    ) {
        for pattern in matches {
            self.bind_pattern(pattern);
        }
        for clause in optional_clauses {
            for pattern in &clause.patterns {
                self.bind_pattern(pattern);
            }
        }
        for unwind in unwinds {
            self.bind(&unwind.alias, VarKind::Scalar);
        }
    }

    /// A `WITH` replaces the scope with its items.
    fn after_with(&mut self, items: &[RetItem]) {
        let mut next = Scope::default();
        for item in items {
            let carried = match &item.value {
                RetVal::Var(name) => self.kind(name),
                _ => None,
            };
            let name = item.alias.clone().or_else(|| match &item.value {
                RetVal::Var(name) => Some(name.clone()),
                _ => None,
            });
            if let Some(name) = name {
                next.bind(&name, carried.unwrap_or(VarKind::Scalar));
            }
        }
        *self = next;
    }

    /// `WITH` items that carry every node and value in scope into the next stage.
    /// A relationship variable is not carried.
    fn carry_items(&self) -> Vec<RetItem> {
        self.vars
            .iter()
            .filter(|(_, kind)| *kind != VarKind::Rel)
            .map(|(name, _)| RetItem {
                value: RetVal::Var(name.clone()),
                alias: None,
            })
            .collect()
    }

    /// The names that hold a node or a value.
    fn carried_names(&self) -> Vec<String> {
        self.vars
            .iter()
            .filter(|(_, kind)| *kind != VarKind::Rel)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

const READ_AFTER_WRITE: &str = "not supported in mushroomdb 0.10: a MATCH, UNWIND or WITH clause \
     after a write clause in the same statement; put the reads first";

impl<'a> Parser<'a> {
    /// The read clauses after an UNWIND: `MATCH`, `OPTIONAL MATCH`, `UNWIND`
    /// and `WITH`, each stage pushed onto `stages`. A run of MATCH / OPTIONAL
    /// MATCH / UNWIND clauses (and their WHERE) becomes one stage that opens by
    /// carrying every name in `scope`, as `WITH a, b MATCH …` does.
    pub(super) fn stages_after_unwind(
        &mut self,
        scope: &mut Scope,
        stages: &mut Vec<WithStage>,
    ) -> Result<(), String> {
        loop {
            match self.peek() {
                Some(Tok::Match) | Some(Tok::Optional) | Some(Tok::Unwind) => {
                    let items = scope.carry_items();
                    let (matches, optional_clauses, unwinds, post_where) =
                        self.with_stage_tail()?;
                    scope.bind_clauses(&matches, &optional_clauses, &unwinds);
                    stages.push(WithStage {
                        items,
                        where_expr: None,
                        order_by: Vec::new(),
                        skip: None,
                        limit: None,
                        matches,
                        optional_clauses,
                        unwinds,
                        post_where,
                    });
                }
                Some(Tok::With) => {
                    let stage = self.with_stage()?;
                    scope.after_with(&stage.items);
                    scope.bind_clauses(&stage.matches, &stage.optional_clauses, &stage.unwinds);
                    stages.push(stage);
                }
                _ => return Ok(()),
            }
        }
    }

    /// `UNWIND … [read clauses] <write clause>+ [RETURN …]`.
    pub(super) fn unwind_write_stmt(&mut self) -> Result<WriteStatement, String> {
        let mut scope = Scope::default();
        let mut unwinds = Vec::new();
        while self.peek() == Some(&Tok::Unwind) {
            let clause = self.unwind_clause()?;
            scope.bind(&clause.alias, VarKind::Scalar);
            unwinds.push(clause);
        }
        let post_unwind_where = if self.eat(&Tok::Where) {
            Some(self.expr(0)?)
        } else {
            None
        };
        let mut stages = Vec::new();
        self.stages_after_unwind(&mut scope, &mut stages)?;
        let read_scope = scope.carried_names();

        let mut writes: Vec<RowWrite> = Vec::new();
        let mut anonymous = 0usize;
        loop {
            match self.peek() {
                Some(Tok::Create) => {
                    let write = self.row_create(&mut scope, &mut anonymous)?;
                    writes.push(write);
                }
                Some(Tok::Merge) => {
                    let write = self.row_merge(&mut scope, &mut anonymous)?;
                    writes.push(write);
                }
                Some(Tok::Set) => {
                    let write = self.row_set(&scope)?;
                    writes.push(write);
                }
                Some(Tok::Detach) => {
                    self.pos += 1;
                    self.expect(&Tok::Delete, "expected DELETE after DETACH")?;
                    let vars = self.row_delete_targets(&scope)?;
                    writes.push(RowWrite::Delete { vars, detach: true });
                }
                Some(Tok::Delete) => {
                    self.pos += 1;
                    let vars = self.row_delete_targets(&scope)?;
                    writes.push(RowWrite::Delete {
                        vars,
                        detach: false,
                    });
                }
                _ if self.peek_kw("remove") => {
                    self.pos += 1;
                    let removals = self.row_remove_items(&scope)?;
                    writes.push(RowWrite::Remove(removals));
                }
                Some(Tok::Match) | Some(Tok::Optional) | Some(Tok::Unwind) | Some(Tok::With)
                    if !writes.is_empty() =>
                {
                    return Err(READ_AFTER_WRITE.to_string());
                }
                _ => break,
            }
        }
        if writes.is_empty() {
            return Err(self.err(
                "expected CREATE, MERGE, SET, REMOVE or DELETE after the UNWIND clauses \
                 (a statement with no write clause is a read query)",
            ));
        }
        let returns = if self.peek() == Some(&Tok::Return) {
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
            Some(RowReturn {
                items,
                distinct,
                order_by,
                skip,
                limit,
            })
        } else {
            None
        };
        if self.pos < self.toks.len() {
            return Err(self.unsupported_or_unexpected("unexpected tokens after the write clauses"));
        }
        Ok(WriteStatement::UnwindWrite(UnwindWriteStmt {
            read: Query {
                matches: Vec::new(),
                optional_clauses: Vec::new(),
                where_expr: None,
                unwinds,
                post_unwind_where,
                stages,
                returns: Vec::new(),
                distinct: false,
                order_by: Vec::new(),
                skip: None,
                limit: None,
            },
            scope: read_scope,
            writes,
            returns,
        }))
    }

    /// `{key: expr, …}`: property values of a per-row CREATE or MERGE.
    fn row_props(&mut self) -> Result<Vec<(String, Operand)>, String> {
        self.expect(&Tok::LBrace, "expected '{'")?;
        let mut out: Vec<(String, Operand)> = Vec::new();
        if self.eat(&Tok::RBrace) {
            return Ok(out);
        }
        loop {
            let key = self.ident("expected property key")?;
            if out.iter().any(|(k, _)| *k == key) {
                return Err(self.err(&format!("duplicate property key `{key}`")));
            }
            self.expect(&Tok::Colon, "expected ':' after property key")?;
            let value = self.arith_expr()?;
            out.push((key, value));
            if self.eat(&Tok::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Tok::RBrace, "expected '}' to close property map")?;
        Ok(out)
    }

    /// A fresh name for an anonymous node of a per-row write.
    fn anonymous_var(anonymous: &mut usize) -> String {
        let name = format!("_uw{anonymous}");
        *anonymous += 1;
        name
    }

    /// `CREATE pattern [, pattern …]`.
    fn row_create(&mut self, scope: &mut Scope, anonymous: &mut usize) -> Result<RowWrite, String> {
        self.expect(&Tok::Create, "expected CREATE")?;
        let mut nodes: Vec<RowCreateNode> = Vec::new();
        let mut edges: Vec<CreateEdge> = Vec::new();
        loop {
            let mut prev = self.row_create_node(scope, anonymous, &mut nodes)?;
            while matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
                let (etype, next_is_src) = self.create_rel()?;
                let next = self.row_create_node(scope, anonymous, &mut nodes)?;
                let (src_var, dst_var) = if next_is_src {
                    (next.clone(), prev)
                } else {
                    (prev, next.clone())
                };
                edges.push(CreateEdge {
                    src_var,
                    etype,
                    dst_var,
                });
                prev = next;
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(RowWrite::Create { nodes, edges })
    }

    /// One node of a per-row CREATE: `(v:Label {…})` creates it; `(v)` names a
    /// node already bound. Returns the variable.
    fn row_create_node(
        &mut self,
        scope: &mut Scope,
        anonymous: &mut usize,
        nodes: &mut Vec<RowCreateNode>,
    ) -> Result<String, String> {
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
        if self.peek() == Some(&Tok::RParen) {
            self.pos += 1;
            // `(v)`: a reference, which must be a node already bound.
            return match var {
                Some(v) if scope.kind(&v) == Some(VarKind::Node) => Ok(v),
                Some(v) => Err(format!(
                    "CREATE ({v}): `{v}` is not a node bound by an earlier clause; \
                     a new node needs a label, e.g. ({v}:Label {{id: …}})"
                )),
                None => Err(self.err("CREATE node requires a label (e.g., (n:Label {…}))")),
            };
        }
        if !self.eat(&Tok::Colon) {
            return Err(self.err("CREATE node requires a label (e.g., (n:Label {…}))"));
        }
        let label = self.ident("expected label identifier after ':'")?;
        let props = if self.peek() == Some(&Tok::LBrace) {
            self.row_props()?
        } else {
            Vec::new()
        };
        self.expect(&Tok::RParen, "expected ')' to close CREATE node pattern")?;
        let var = match var {
            Some(v) if scope.kind(&v).is_some() => {
                return Err(format!(
                    "CREATE ({v}:{label}): `{v}` is already bound; use a new name for a new node, \
                     or ({v}) to refer to the bound one"
                ))
            }
            Some(v) => v,
            None => Self::anonymous_var(anonymous),
        };
        scope.bind(&var, VarKind::Node);
        nodes.push(RowCreateNode {
            var: var.clone(),
            label,
            props,
        });
        Ok(var)
    }

    /// `MERGE node [-[:T]-> node] [ON CREATE SET …] [ON MATCH SET …]`.
    fn row_merge(&mut self, scope: &mut Scope, anonymous: &mut usize) -> Result<RowWrite, String> {
        self.expect(&Tok::Merge, "expected MERGE")?;
        let start = self.merge_node_with("expected '(' after MERGE", Self::row_props)?;
        let start = self.row_merge_node(start, scope, anonymous)?;
        let edge = if matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
            let (rel_var, etype, dir) = self.merge_rel()?;
            let end =
                self.merge_node_with("expected '(' to start the MERGE end node", Self::row_props)?;
            let end = self.row_merge_node(end, scope, anonymous)?;
            if matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
                return Err(
                    "not supported in mushroomdb 0.10: MERGE with more than one relationship"
                        .to_string(),
                );
            }
            if let Some(var) = &rel_var {
                scope.bind(var, VarKind::Rel);
            }
            Some(RowMergeEdge {
                etype,
                dir,
                rel_var,
                end,
            })
        } else {
            None
        };
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        while self.eat_ident_kw("on") {
            if self.eat(&Tok::Create) {
                self.expect(&Tok::Set, "expected SET after ON CREATE")?;
                on_create.extend(self.merge_set_clauses_with(true)?);
            } else if self.eat(&Tok::Match) {
                self.expect(&Tok::Set, "expected SET after ON MATCH")?;
                on_match.extend(self.merge_set_clauses_with(true)?);
            } else {
                return Err(self.err("expected CREATE or MATCH after ON"));
            }
        }
        for sc in on_create.iter().chain(&on_match) {
            let names_a_node =
                sc.var == start.var || edge.as_ref().is_some_and(|e| e.end.var == sc.var);
            if !names_a_node {
                return Err(format!(
                    "SET variable '{}' does not match a MERGE node variable \
                     (a relationship takes no SET)",
                    sc.var
                ));
            }
        }
        Ok(RowWrite::Merge(Box::new(RowMerge {
            start,
            edge,
            on_create,
            on_match,
        })))
    }

    /// A parsed MERGE node as a [`RowMergeNode`], naming it when anonymous and
    /// binding its variable.
    fn row_merge_node(
        &mut self,
        node: super::MergeNode<Operand>,
        scope: &mut Scope,
        anonymous: &mut usize,
    ) -> Result<RowMergeNode, String> {
        let var = match node.var {
            Some(v) if scope.kind(&v).is_some() => {
                return Err(format!(
                    "MERGE ({v}:{}): `{v}` is already bound; use a new name for the MERGE node",
                    node.label
                ))
            }
            Some(v) => v,
            None => Self::anonymous_var(anonymous),
        };
        scope.bind(&var, VarKind::Node);
        Ok(RowMergeNode {
            var,
            label: node.label,
            key_field: node.key_field,
            key: node.key_value,
            ns: node.ns,
        })
    }

    /// `SET n.f = expr [, …]` or `SET n = map` / `SET n += map`.
    fn row_set(&mut self, scope: &Scope) -> Result<RowWrite, String> {
        if self.set_map_form_ahead() {
            self.pos += 1; // SET
            let var = self.ident("expected variable after SET")?;
            let merge = self.eat(&Tok::Plus);
            self.expect(&Tok::Eq, "expected '=' in SET clause")?;
            self.require_node(scope, &var, "SET")?;
            let value = self.arith_expr()?;
            if self.peek() == Some(&Tok::Comma) {
                return Err(
                    "not supported in mushroomdb 0.10: a SET map form followed by another SET item; \
                     use one SET clause per map"
                        .to_string(),
                );
            }
            return Ok(RowWrite::SetMap { var, merge, value });
        }
        self.expect(&Tok::Set, "expected SET")?;
        let mut sets: Vec<SetClause> = Vec::new();
        loop {
            let clause = self.set_clause_with(true)?;
            self.require_node(scope, &clause.var, "SET")?;
            sets.push(clause);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(RowWrite::Set(sets))
    }

    /// `var.field [, …]` after REMOVE; each `var` a node in scope.
    fn row_remove_items(&mut self, scope: &Scope) -> Result<Vec<(String, String)>, String> {
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
            self.require_node(scope, &var, "REMOVE")?;
            items.push((var, field));
            if !self.eat(&Tok::Comma) {
                return Ok(items);
            }
        }
    }

    /// Node variables after `[DETACH] DELETE`. A relationship variable is
    /// refused by name.
    fn row_delete_targets(&mut self, scope: &Scope) -> Result<Vec<String>, String> {
        let mut vars = Vec::new();
        loop {
            let var = self.ident("expected node variable to DELETE")?;
            match scope.kind(&var) {
                Some(VarKind::Node) => vars.push(var),
                Some(VarKind::Rel) => {
                    return Err(format!(
                        "not supported in mushroomdb 0.10: DELETE of the relationship `{var}` \
                         after UNWIND; delete nodes, or use MATCH … DELETE"
                    ))
                }
                _ => {
                    return Err(format!(
                        "DELETE `{var}`: not a node variable bound by an earlier clause"
                    ))
                }
            }
            if !self.eat(&Tok::Comma) {
                return Ok(vars);
            }
        }
    }

    fn require_node(&self, scope: &Scope, var: &str, clause: &str) -> Result<(), String> {
        match scope.kind(var) {
            Some(VarKind::Node) => Ok(()),
            Some(VarKind::Rel) => Err(format!(
                "{clause} {var}: '{var}' is a relationship; {clause} works on nodes here"
            )),
            Some(VarKind::Scalar) => Err(format!(
                "{clause} {var}: '{var}' is a value, not a node variable"
            )),
            None => Err(format!(
                "{clause} {var}: '{var}' is not bound by an earlier clause"
            )),
        }
    }
}
