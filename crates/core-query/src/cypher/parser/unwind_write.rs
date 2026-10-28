//! Clauses after `UNWIND` (QL-09b): the read clauses that may follow it
//! (`MATCH`, `OPTIONAL MATCH`, `UNWIND`, `WITH`) in both read queries and write
//! statements, and the write statements that begin with `UNWIND`:
//! `UNWIND $rows AS r CREATE …`, `… MERGE …`, `… MATCH … SET …`, and so on.

use super::Parser;
use crate::cypher::ast::{
    CreateEdge, MergeSet, OptionalClause, Pattern, RelDir, RetItem, RetVal, RowCreateNode,
    RowMerge, RowMergeEdge, RowMergeNode, RowReturn, RowWrite, SetClause, UnwindClause, UnwindExpr,
    UnwindWriteStmt, WithStage, WriteStatement, ONE_ROW_VAR,
};
use crate::cypher::{Operand, Query, Tok};
use core_storage::Value;

/// What a name in scope holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VarKind {
    Node,
    Rel,
    /// A value: an UNWIND alias, a `WITH` alias or any other expression.
    Scalar,
    /// A named path (`MATCH p = …`).
    Path,
}

/// The names in scope at a point of a read part, with what each holds.
#[derive(Debug, Default)]
pub(super) struct Scope {
    vars: Vec<(String, VarKind)>,
    /// Names the parser made up (an anonymous node of a write clause, the
    /// one-row UNWIND alias): in scope, but not listed by `RETURN *`.
    hidden: Vec<String>,
    /// Variable-length relationship names: bound to a hop count, never
    /// returned bare, so not listed by `RETURN *`.
    var_length: Vec<String>,
}

impl Scope {
    pub(super) fn bind(&mut self, name: &str, kind: VarKind) {
        match self.vars.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = kind,
            None => self.vars.push((name.to_string(), kind)),
        }
    }

    /// Bind a name the parser made up.
    pub(super) fn bind_hidden(&mut self, name: &str, kind: VarKind) {
        self.bind(name, kind);
        if !self.hidden.iter().any(|h| h == name) {
            self.hidden.push(name.to_string());
        }
    }

    fn kind(&self, name: &str) -> Option<VarKind> {
        self.vars.iter().find(|(n, _)| n == name).map(|(_, k)| *k)
    }

    /// The items `RETURN *` / `WITH *` stands for: every node, relationship,
    /// path and value name the query bound that is in scope here, in
    /// alphabetical order. Variable-length relationship variables are left
    /// out, since they are a hop count and not returned bare; so are names
    /// the parser made up. An error when nothing is left.
    pub(super) fn star_items(&self) -> Result<Vec<RetItem>, String> {
        let mut names: Vec<&String> = self
            .vars
            .iter()
            .filter(|(name, kind)| {
                let var_length = *kind == VarKind::Rel && self.var_length.contains(name);
                !var_length && !self.hidden.contains(name)
            })
            .map(|(name, _)| name)
            .collect();
        if names.is_empty() {
            return Err("RETURN * / WITH * with no variables in scope".to_string());
        }
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| RetItem {
                value: RetVal::Var(name.clone()),
                alias: None,
            })
            .collect())
    }

    fn bind_pattern(&mut self, pattern: &Pattern) {
        if let Some(var) = &pattern.path_var {
            self.bind(var, VarKind::Path);
        }
        if let Some(var) = &pattern.start.var {
            self.bind(var, VarKind::Node);
        }
        for (rel, dest) in &pattern.chain {
            if let Some(var) = &rel.var {
                self.bind(var, VarKind::Rel);
                if rel.hops.is_some() && !self.var_length.contains(var) {
                    self.var_length.push(var.clone());
                }
            }
            if let Some(var) = &dest.var {
                self.bind(var, VarKind::Node);
            }
        }
    }

    /// The names the MATCH patterns of a statement bind.
    pub(super) fn of_patterns(patterns: &[Pattern]) -> Scope {
        let mut scope = Scope::default();
        scope.bind_clauses(patterns, &[], &[]);
        scope
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
    pub(super) fn after_with(&mut self, items: &[RetItem]) {
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
    /// A relationship or path variable is not carried.
    fn carry_items(&self) -> Vec<RetItem> {
        self.vars
            .iter()
            .filter(|(_, kind)| matches!(kind, VarKind::Node | VarKind::Scalar))
            .map(|(name, _)| RetItem {
                value: RetVal::Var(name.clone()),
                alias: None,
            })
            .collect()
    }

    /// The names that hold a node.
    fn node_names(&self) -> Vec<String> {
        self.vars
            .iter()
            .filter(|(_, kind)| *kind == VarKind::Node)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The names that hold a relationship.
    fn rel_names(&self) -> Vec<String> {
        self.vars
            .iter()
            .filter(|(_, kind)| *kind == VarKind::Rel)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The names that hold a node or a value.
    fn carried_names(&self) -> Vec<String> {
        self.vars
            .iter()
            .filter(|(_, kind)| matches!(kind, VarKind::Node | VarKind::Scalar))
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// The relationship of a per-row CREATE, as written.
struct CreateRel {
    var: Option<String>,
    etype: String,
    /// `<-[…]-`: the node after the relationship is its start.
    next_is_src: bool,
    props: Vec<(String, Operand)>,
    props_map: Option<Operand>,
}

/// `UNWIND [1] AS <ONE_ROW_VAR>`: one row for the clauses after it.
pub(super) fn one_row_unwind() -> UnwindClause {
    UnwindClause {
        list: UnwindExpr::Lit(vec![Value::Int(1)]),
        alias: ONE_ROW_VAR.to_string(),
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
                    let stage = self.with_stage(scope)?;
                    scope.after_with(&stage.items);
                    scope.bind_clauses(&stage.matches, &stage.optional_clauses, &stage.unwinds);
                    stages.push(stage);
                }
                _ => return Ok(()),
            }
        }
    }

    /// `UNWIND … [read clauses] <write clause>+ [RETURN …]`, or the same
    /// statement opening with `MATCH` (`MATCH … CREATE …`): that one is read as
    /// `UNWIND [1] AS <ONE_ROW_VAR> MATCH …`, so it runs once per matched row
    /// exactly as the `UNWIND` form does.
    pub(super) fn unwind_write_stmt(&mut self) -> Result<WriteStatement, String> {
        let mut scope = Scope::default();
        let mut unwinds = Vec::new();
        if self.peek() != Some(&Tok::Unwind) {
            scope.bind_hidden(ONE_ROW_VAR, VarKind::Scalar);
            unwinds.push(one_row_unwind());
        }
        while self.peek() == Some(&Tok::Unwind) {
            let clause = self.unwind_clause()?;
            scope.bind(&clause.alias, VarKind::Scalar);
            unwinds.push(clause);
        }
        let post_unwind_where = if self.eat(&Tok::Where) {
            Some(self.where_expr()?)
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
                    let (vars, rels) = self.row_delete_targets(&scope)?;
                    writes.push(RowWrite::Delete {
                        vars,
                        rels,
                        detach: true,
                    });
                }
                Some(Tok::Delete) => {
                    self.pos += 1;
                    let (vars, rels) = self.row_delete_targets(&scope)?;
                    writes.push(RowWrite::Delete {
                        vars,
                        rels,
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
            let (distinct, items) = self.return_clause(&scope)?;
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
        Ok(WriteStatement::UnwindWrite(Box::new(UnwindWriteStmt {
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
            node_vars: scope.node_names(),
            rel_vars: scope.rel_names(),
            writes,
            returns,
        })))
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
            let value = self.value_expr()?;
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
            let created_before = nodes.len();
            let mut prev = self.row_create_node(scope, anonymous, &mut nodes)?;
            // `CREATE (a)` alone, with `a` bound, would create nothing: only
            // an end of a new relationship may name a bound node.
            if nodes.len() == created_before
                && !matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt))
            {
                return Err(crate::cypher::check::refuse(
                    crate::cypher::check::VARIABLE_ALREADY_BOUND,
                    format!(
                        "CREATE ({prev}): `{prev}` is already bound; CREATE makes new nodes, \
                         so name a new one or use the bound one as an end of a new relationship"
                    ),
                ));
            }
            while matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
                let rel = self.row_create_rel()?;
                let next = self.row_create_node(scope, anonymous, &mut nodes)?;
                let (src_var, dst_var) = if rel.next_is_src {
                    (next.clone(), prev)
                } else {
                    (prev, next.clone())
                };
                if let Some(var) = &rel.var {
                    if scope.kind(var).is_some() {
                        return Err(format!(
                            "CREATE ()-[{var}:{}]->(): `{var}` is already bound; use a new name \
                             for a new relationship",
                            rel.etype
                        ));
                    }
                    scope.bind(var, VarKind::Rel);
                }
                edges.push(CreateEdge {
                    src_var,
                    etype: rel.etype,
                    dst_var,
                    rel_var: rel.var,
                    props: rel.props,
                    props_map: rel.props_map,
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
            if let Some(v) = &var {
                match scope.kind(v) {
                    // `(v)`: a reference to a node already bound.
                    Some(VarKind::Node) => {
                        self.pos += 1;
                        return Ok(v.clone());
                    }
                    Some(_) => {
                        return Err(format!(
                            "CREATE ({v}): `{v}` is already bound to a value that is not a node"
                        ))
                    }
                    // `(v)` with `v` new: a new node with no label (QL-NM).
                    None => {}
                }
            }
        }
        // A node needs no label (QL-NM): `()`, `(v)`, `({k: v})` are new nodes
        // stored under no label.
        let label = if self.eat(&Tok::Colon) {
            self.label_name()?
        } else {
            String::new()
        };
        let mut props_map = None;
        let props = match self.peek() {
            Some(Tok::LBrace) => self.row_props()?,
            // `(n:L $props)`: the whole map is the parameter.
            Some(Tok::Param(name)) => {
                props_map = Some(Operand::Param(name.clone()));
                self.pos += 1;
                Vec::new()
            }
            _ => Vec::new(),
        };
        self.expect(&Tok::RParen, "expected ')' to close CREATE node pattern")?;
        let var = match var {
            Some(v) if scope.kind(&v).is_some() => {
                return Err(format!(
                    "CREATE ({v}:{label}): `{v}` is already bound; use a new name for a new node, \
                     or ({v}) to refer to the bound one"
                ))
            }
            Some(v) => {
                scope.bind(&v, VarKind::Node);
                v
            }
            None => {
                let v = Self::anonymous_var(anonymous);
                scope.bind_hidden(&v, VarKind::Node);
                v
            }
        };
        nodes.push(RowCreateNode {
            var: var.clone(),
            label,
            props,
            props_map,
        });
        Ok(var)
    }

    /// `MERGE node [-[:T]-> node] [ON CREATE SET …] [ON MATCH SET …]`.
    fn row_merge(&mut self, scope: &mut Scope, anonymous: &mut usize) -> Result<RowWrite, String> {
        self.expect(&Tok::Merge, "expected MERGE")?;
        let start = match self.bound_merge_node(scope) {
            Some(node) => node,
            None => {
                let start = self.merge_node_with("expected '(' after MERGE", Self::row_props)?;
                self.row_merge_node(start, scope, anonymous)?
            }
        };
        let edge = if matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
            let (rel_var, etype, dir, props) = self.row_merge_rel()?;
            let end = match self.bound_merge_node(scope) {
                Some(node) => node,
                None => {
                    let end = self.merge_node_with(
                        "expected '(' to start the MERGE end node",
                        Self::row_props,
                    )?;
                    self.row_merge_node(end, scope, anonymous)?
                }
            };
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
                props,
            })
        } else {
            if start.bound {
                return Err(format!(
                    "MERGE ({0}): `{0}` is already bound, so the MERGE matches it as it is; \
                     MERGE a relationship from it, e.g. MERGE ({0})-[:T]->(b:Label {{id: …}})",
                    start.var
                ));
            }
            None
        };
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        while self.eat_ident_kw("on") {
            if self.eat(&Tok::Create) {
                self.expect(&Tok::Set, "expected SET after ON CREATE")?;
                on_create.extend(self.row_merge_sets()?);
            } else if self.eat(&Tok::Match) {
                self.expect(&Tok::Set, "expected SET after ON MATCH")?;
                on_match.extend(self.row_merge_sets()?);
            } else {
                return Err(self.err("expected CREATE or MATCH after ON"));
            }
        }
        for item in on_create.iter().chain(&on_match) {
            let var = item.var();
            let names_a_node = var == start.var || edge.as_ref().is_some_and(|e| e.end.var == var);
            let names_the_rel = edge
                .as_ref()
                .is_some_and(|e| e.rel_var.as_deref() == Some(var));
            if !names_a_node && !names_the_rel {
                return Err(format!(
                    "SET variable '{var}' does not match a MERGE node or relationship variable"
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

    /// The items after a per-row MERGE's `ON CREATE SET` / `ON MATCH SET`:
    /// `n.f = expr`, `n = <map>` and `n += <map>`, in any mix.
    fn row_merge_sets(&mut self) -> Result<Vec<MergeSet>, String> {
        let mut items = Vec::new();
        loop {
            if matches!(self.peek(), Some(Tok::Ident(_)))
                && matches!(
                    (self.toks.get(self.pos + 1), self.toks.get(self.pos + 2)),
                    (Some(Tok::Eq), _) | (Some(Tok::Plus), Some(Tok::Eq))
                )
            {
                let var = self.ident("expected variable after SET")?;
                let merge = self.eat(&Tok::Plus);
                self.expect(&Tok::Eq, "expected '=' in SET clause")?;
                let value = self.value_expr()?;
                items.push(MergeSet::Map { var, merge, value });
            } else {
                items.push(MergeSet::Prop(self.set_clause_with(true)?));
            }
            if !self.eat(&Tok::Comma) {
                return Ok(items);
            }
        }
    }

    /// `(v)` at the cursor, where `v` is a node an earlier clause bound: a MERGE
    /// end that is used as it is. `None` (consuming nothing) for anything else.
    fn bound_merge_node(&mut self, scope: &Scope) -> Option<RowMergeNode> {
        let (Some(Tok::LParen), Some(Tok::Ident(var)), Some(Tok::RParen)) = (
            self.peek(),
            self.toks.get(self.pos + 1),
            self.toks.get(self.pos + 2),
        ) else {
            return None;
        };
        if scope.kind(var) != Some(VarKind::Node) {
            return None;
        }
        self.pos += 3;
        Some(RowMergeNode {
            var: var.clone(),
            label: String::new(),
            key_field: String::new(),
            key: Operand::Var(var.clone()),
            props: Vec::new(),
            key_pattern: false,
            ns: None,
            bound: true,
        })
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
            Some(v) => {
                scope.bind(&v, VarKind::Node);
                v
            }
            None => {
                let v = Self::anonymous_var(anonymous);
                scope.bind_hidden(&v, VarKind::Node);
                v
            }
        };
        let key_pattern = node.key.is_some();
        let (key_field, key) = node
            .key
            .unwrap_or_else(|| (String::new(), Operand::Lit(Value::Null)));
        Ok(RowMergeNode {
            var,
            label: node.label,
            key_field,
            key,
            props: node.props,
            key_pattern,
            ns: node.ns,
            bound: false,
        })
    }

    /// `SET n.f = expr [, …]` or `SET n = map` / `SET n += map`.
    fn row_set(&mut self, scope: &Scope) -> Result<RowWrite, String> {
        if self.set_map_form_ahead() {
            self.pos += 1; // SET
            let var = self.ident("expected variable after SET")?;
            let merge = self.eat(&Tok::Plus);
            self.expect(&Tok::Eq, "expected '=' in SET clause")?;
            self.require_entity(scope, &var, "SET")?;
            let value = self.value_expr()?;
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
            self.require_entity(scope, &clause.var, "SET")?;
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
            self.require_entity(scope, &var, "REMOVE")?;
            items.push((var, field));
            if !self.eat(&Tok::Comma) {
                return Ok(items);
            }
        }
    }

    /// The node variables, and the relationship variables, after
    /// `[DETACH] DELETE`. A relationship variable is refused by name, unless
    /// the statement has an `OPTIONAL MATCH` (QL-ON): there it can be null, and
    /// `DELETE r` on a null is skipped.
    fn row_delete_targets(&mut self, scope: &Scope) -> Result<(Vec<String>, Vec<String>), String> {
        let optional = self.toks.contains(&Tok::Optional);
        let mut vars = Vec::new();
        let mut rels = Vec::new();
        loop {
            let var = self.ident("expected node variable to DELETE")?;
            match scope.kind(&var) {
                Some(VarKind::Node) => vars.push(var),
                Some(VarKind::Rel) if optional => rels.push(var),
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
                return Ok((vars, rels));
            }
        }
    }

    /// [`require_node`](Self::require_node), or a single relationship: `SET`
    /// and `REMOVE` write a relationship's properties as they write a node's.
    fn require_entity(&self, scope: &Scope, var: &str, clause: &str) -> Result<(), String> {
        match scope.kind(var) {
            Some(VarKind::Rel) => Ok(()),
            _ => self.require_node(scope, var, clause),
        }
    }

    /// The relationship of a per-row CREATE: `-[v:T {…}]->`, `<-[:T $p]-`.
    fn row_create_rel(&mut self) -> Result<CreateRel, String> {
        let (rel, props, props_map) = self.rel_with_props()?;
        if rel.hops.is_some() {
            return Err(super::refusal("CREATE with a variable-length relationship"));
        }
        let etype = match rel.etypes.as_slice() {
            [one] => one.clone(),
            [] => return Err("expected ':TYPE' in CREATE relationship".to_string()),
            _ => return Err(super::refusal("CREATE with alternative relationship types")),
        };
        let next_is_src = match rel.dir {
            RelDir::Left => true,
            RelDir::Right => false,
            RelDir::Undirected => {
                return Err("expected '>' — CREATE requires directed relationships".to_string())
            }
        };
        Ok(CreateRel {
            var: rel.var,
            etype,
            next_is_src,
            props,
            props_map,
        })
    }

    /// The relationship of a per-row MERGE, with its property map: one typed,
    /// directed, single-hop relationship, as [`merge_rel`](Self::merge_rel).
    #[allow(clippy::type_complexity)]
    fn row_merge_rel(
        &mut self,
    ) -> Result<(Option<String>, String, RelDir, Vec<(String, Operand)>), String> {
        let (rel, props, props_map) = self.rel_with_props()?;
        if props_map.is_some() {
            return Err(super::refusal(
                "a $parameter map in a MERGE relationship; name its properties, \
                 e.g. MERGE (a)-[r:T {k: $p.k}]->(b) ON CREATE SET r += $p",
            ));
        }
        let (var, etype, dir) = self.merge_rel_checked(rel)?;
        Ok((var, etype, dir, props))
    }

    /// A relationship pattern whose bracket may end with a property map or a
    /// `$parameter` map: `-[r:T {k: v}]->`, `<-[:T $p]-`.
    #[allow(clippy::type_complexity)]
    fn rel_with_props(
        &mut self,
    ) -> Result<
        (
            crate::cypher::RelPat,
            Vec<(String, Operand)>,
            Option<Operand>,
        ),
        String,
    > {
        let left = self.eat(&Tok::Lt);
        self.expect(&Tok::Dash, "expected '-' to start a relationship")?;
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
        let mut etypes = Vec::new();
        if self.eat(&Tok::Colon) {
            etypes.push(self.ident("expected relationship type identifier after ':'")?);
            while self.eat(&Tok::Pipe) {
                self.eat(&Tok::Colon);
                etypes.push(self.ident("expected relationship type identifier after '|'")?);
            }
        }
        let hops = if self.eat(&Tok::Star) {
            Some(self.parse_hop_range()?)
        } else {
            None
        };
        let mut props_map = None;
        let props = match self.peek() {
            Some(Tok::LBrace) => self.row_props()?,
            Some(Tok::Param(name)) => {
                props_map = Some(Operand::Param(name.clone()));
                self.pos += 1;
                Vec::new()
            }
            _ => Vec::new(),
        };
        self.expect(
            &Tok::RBracket,
            "expected ']' to close a relationship pattern",
        )?;
        self.expect(&Tok::Dash, "expected '-' after ']'")?;
        let dir = if left {
            RelDir::Left
        } else if self.eat(&Tok::Gt) {
            RelDir::Right
        } else {
            RelDir::Undirected
        };
        Ok((
            crate::cypher::RelPat {
                var,
                etypes,
                dir,
                hops,
            },
            props,
            props_map,
        ))
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
            Some(VarKind::Path) => Err(format!(
                "{clause} {var}: '{var}' is a path, not a node variable"
            )),
            None => Err(format!(
                "{clause} {var}: '{var}' is not bound by an earlier clause"
            )),
        }
    }
}
