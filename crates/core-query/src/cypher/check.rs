//! The compile-time semantic check (QL-VE): openCypher's validation errors,
//! raised after a statement parses and before it is planned or reads a row.
//!
//! The parse entry points ([`parse`](super::parse),
//! [`parse_read`](super::parse_read), [`parse_write`](super::parse_write)) run
//! it on the tree they built, so every caller (each read and write entry point,
//! subscriptions, transactions) gets the same answer. A refusal's message opens
//! with the openCypher error kind and a colon (`VariableTypeConflict: …`),
//! names the variable or function, and is reported as a `parse:` error.
//!
//! The check only refuses what is wrong whatever the data: a name whose kind
//! is not known at compile time (an `UNWIND` alias, a parameter, a function's
//! result) is never refused for its kind.

use super::ast::{
    AggArg, AggFunc, Expr, NodePat, Operand, OrderItem, OrderTarget, Pattern, Query, RetItem,
    RetVal, UnwindExpr, WithStage, WriteStatement, LABEL_TEST_FN, REL_ENDS_FN,
};
use super::parser::UnionQuery;
use core_storage::Value;
use std::collections::BTreeMap;

/// A name bound as one kind of thing and used as another (a node and a
/// relationship, a path and a node, a value and a node).
pub const VARIABLE_TYPE_CONFLICT: &str = "VariableTypeConflict";
/// A name that is already bound, declared again where only a new name may go.
pub const VARIABLE_ALREADY_BOUND: &str = "VariableAlreadyBound";
/// An aggregate where no aggregation is allowed.
pub const INVALID_AGGREGATION: &str = "InvalidAggregation";
/// A `WITH` aggregate with no `AS` name.
pub const NO_EXPRESSION_ALIAS: &str = "NoExpressionAlias";
/// Two `WITH` items with the same name.
pub const COLUMN_NAME_CONFLICT: &str = "ColumnNameConflict";
/// `UNION` and `UNION ALL` in one statement.
pub const INVALID_CLAUSE_COMPOSITION: &str = "InvalidClauseComposition";
/// `UNION` parts that return different columns.
pub const DIFFERENT_COLUMNS_IN_UNION: &str = "DifferentColumnsInUnion";
/// A call of a function that does not exist.
pub const UNKNOWN_FUNCTION: &str = "UnknownFunction";
/// An argument (or a property access, or an `IN` list) of a kind the
/// operation cannot take.
pub const INVALID_ARGUMENT_TYPE: &str = "InvalidArgumentType";

/// Every error kind the check raises.
pub const ERROR_KINDS: &[&str] = &[
    VARIABLE_TYPE_CONFLICT,
    VARIABLE_ALREADY_BOUND,
    INVALID_AGGREGATION,
    NO_EXPRESSION_ALIAS,
    COLUMN_NAME_CONFLICT,
    INVALID_CLAUSE_COMPOSITION,
    DIFFERENT_COLUMNS_IN_UNION,
    UNKNOWN_FUNCTION,
    INVALID_ARGUMENT_TYPE,
];

/// The refusal message: the kind, then what is wrong.
pub(crate) fn refuse(kind: &str, detail: impl std::fmt::Display) -> String {
    format!("{kind}: {detail}")
}

/// The aggregate function names, in lower case.
const AGGREGATES: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "collect",
    "stdev",
    "stdevp",
    "percentilecont",
    "percentiledisc",
];

fn is_aggregate_name(name: &str) -> bool {
    AGGREGATES.iter().any(|a| a.eq_ignore_ascii_case(name))
}

/// The type of a value known at compile time, for the checks that need one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueType {
    /// A number, a string, a boolean or a list: no properties.
    NoProperties,
    /// A map, or a value the check does not type further.
    Other,
}

/// What a name holds, as far as compile time can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Node,
    Rel,
    /// A variable-length relationship (`[r*]`).
    VarRel,
    Path,
    /// A value that is certainly not a node, relationship or path (a
    /// literal, a list or map literal).
    Value(ValueType),
    /// Not known until a row is read: an `UNWIND` alias, a parameter, a
    /// function's result.
    Unknown,
}

impl Kind {
    fn describe(self) -> &'static str {
        match self {
            Kind::Node => "a node",
            Kind::Rel | Kind::VarRel => "a relationship",
            Kind::Path => "a path",
            Kind::Value(_) => "a value",
            Kind::Unknown => "a value",
        }
    }

    fn is_relationship(self) -> bool {
        matches!(self, Kind::Rel | Kind::VarRel)
    }
}

/// The names in scope, with their kinds.
#[derive(Debug, Default, Clone)]
struct Scope {
    vars: BTreeMap<String, Kind>,
}

impl Scope {
    fn get(&self, name: &str) -> Option<Kind> {
        self.vars.get(name).copied()
    }

    fn with_local(&self, names: &[&str]) -> Scope {
        let mut inner = self.clone();
        for name in names {
            inner.vars.insert((*name).to_string(), Kind::Unknown);
        }
        inner
    }
}

/// Check a read query, single or a `UNION` chain.
pub fn check_union(union: &UnionQuery) -> Result<(), String> {
    if union.all_flags.windows(2).any(|w| w[0] != w[1]) {
        return Err(refuse(
            INVALID_CLAUSE_COMPOSITION,
            "UNION and UNION ALL cannot be mixed in one query",
        ));
    }
    for part in &union.parts {
        check_query(part)?;
    }
    let mut columns = union.parts.iter().map(|part| known_columns(&part.returns));
    if let Some(Some(first)) = columns.next() {
        for other in columns.flatten() {
            if other != first {
                return Err(refuse(
                    DIFFERENT_COLUMNS_IN_UNION,
                    format!(
                        "every UNION part must return the same columns; got {first:?} then {other:?}"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// The column names a RETURN gives when every one is known without the
/// planner (an alias, a variable or `var.field`); `None` otherwise.
fn known_columns(items: &[RetItem]) -> Option<Vec<String>> {
    items
        .iter()
        .map(|item| match (&item.alias, &item.value) {
            (Some(alias), _) => Some(alias.clone()),
            (None, RetVal::Var(v)) => Some(v.clone()),
            (None, RetVal::Prop { var, field }) => Some(format!("{var}.{field}")),
            _ => None,
        })
        .collect()
}

/// Check one read query (one part of a `UNION`).
pub fn check_query(q: &Query) -> Result<(), String> {
    let mut scope = Scope::default();
    bind_patterns(&mut scope, &q.matches)?;
    for clause in &q.optional_clauses {
        bind_patterns(&mut scope, &clause.patterns)?;
    }
    for pattern in q
        .matches
        .iter()
        .chain(q.optional_clauses.iter().flat_map(|c| c.patterns.iter()))
    {
        check_pattern_operands(&scope, pattern)?;
    }
    for clause in &q.optional_clauses {
        if let Some(w) = &clause.where_expr {
            check_where(&scope, w)?;
        }
    }
    if let Some(w) = &q.where_expr {
        check_where(&scope, w)?;
    }
    for unwind in &q.unwinds {
        check_unwind(&scope, &unwind.list)?;
        scope.vars.insert(unwind.alias.clone(), Kind::Unknown);
    }
    if let Some(w) = &q.post_unwind_where {
        check_where(&scope, w)?;
    }
    for stage in &q.stages {
        scope = check_stage(scope, stage)?;
    }
    check_items(&scope, &q.returns)?;
    // ORDER BY after RETURN sees RETURN's names: an alias shadows an earlier
    // binding of the same name (`RETURN a AS p ORDER BY p.name`).
    let mut order_scope = scope.clone();
    for item in &q.returns {
        if let Some(alias) = &item.alias {
            order_scope
                .vars
                .insert(alias.clone(), item_kind(&scope, &item.value));
        }
    }
    check_order_by(&order_scope, &q.order_by, &q.returns)?;
    Ok(())
}

/// Check one `WITH` stage and return the scope after it.
fn check_stage(scope: Scope, stage: &WithStage) -> Result<Scope, String> {
    check_items(&scope, &stage.items)?;
    let mut seen: Vec<&str> = Vec::new();
    let mut next = Scope::default();
    for item in &stage.items {
        let name = match (&item.alias, &item.value) {
            (Some(alias), _) => alias.as_str(),
            (None, RetVal::Var(v)) => v.as_str(),
            (None, RetVal::Agg { .. }) => {
                return Err(refuse(
                    NO_EXPRESSION_ALIAS,
                    "an aggregate in WITH must be named with AS (WITH count(*) AS c)",
                ))
            }
            // An unaliased scalar names itself, as it did before the check
            // (`WITH t, t.location[1]` is the column `t.location[1]`).
            (None, _) => continue,
        };
        if seen.contains(&name) {
            return Err(refuse(
                COLUMN_NAME_CONFLICT,
                format!("WITH names `{name}` more than once"),
            ));
        }
        seen.push(name);
        next.vars
            .insert(name.to_string(), item_kind(&scope, &item.value));
    }
    // ORDER BY after WITH sees the names before and after the projection.
    let mut order_scope = scope.clone();
    order_scope
        .vars
        .extend(next.vars.iter().map(|(k, v)| (k.clone(), *v)));
    check_order_by(&order_scope, &stage.order_by, &stage.items)?;
    if let Some(w) = &stage.where_expr {
        check_where(&order_scope, w)?;
    }
    let mut scope = next;
    bind_patterns(&mut scope, &stage.matches)?;
    for clause in &stage.optional_clauses {
        bind_patterns(&mut scope, &clause.patterns)?;
    }
    for pattern in stage.matches.iter().chain(
        stage
            .optional_clauses
            .iter()
            .flat_map(|c| c.patterns.iter()),
    ) {
        check_pattern_operands(&scope, pattern)?;
    }
    for clause in &stage.optional_clauses {
        if let Some(w) = &clause.where_expr {
            check_where(&scope, w)?;
        }
    }
    for unwind in &stage.unwinds {
        check_unwind(&scope, &unwind.list)?;
        scope.vars.insert(unwind.alias.clone(), Kind::Unknown);
    }
    if let Some(w) = &stage.post_where {
        check_where(&scope, w)?;
    }
    Ok(scope)
}

/// The kind a projected item gives its name.
fn item_kind(scope: &Scope, value: &RetVal) -> Kind {
    match value {
        RetVal::Var(v) => scope.get(v).unwrap_or(Kind::Unknown),
        RetVal::ScalarExpr(op) => operand_kind(scope, op),
        _ => Kind::Unknown,
    }
}

/// The kind of an operand, when compile time can tell.
fn operand_kind(scope: &Scope, op: &Operand) -> Kind {
    match op {
        Operand::Lit(Value::Null) => Kind::Unknown,
        Operand::Lit(Value::Map(_)) | Operand::MapLit(_) => Kind::Value(ValueType::Other),
        Operand::Lit(
            Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_) | Value::List(_),
        )
        | Operand::ListLit(_) => Kind::Value(ValueType::NoProperties),
        Operand::Lit(_) => Kind::Value(ValueType::Other),
        Operand::Var(v) => scope.get(v).unwrap_or(Kind::Unknown),
        _ => Kind::Unknown,
    }
}

/// Bind the names of one MATCH (or OPTIONAL MATCH) run of patterns.
fn bind_patterns(scope: &mut Scope, patterns: &[Pattern]) -> Result<(), String> {
    for pattern in patterns {
        bind_pattern(scope, pattern)?;
    }
    Ok(())
}

fn bind_pattern(scope: &mut Scope, pattern: &Pattern) -> Result<(), String> {
    let path = pattern.path_var.as_deref();
    if let Some(p) = path {
        if scope.get(p).is_some() {
            return Err(refuse(
                VARIABLE_ALREADY_BOUND,
                format!("variable `{p}` is already bound; a path needs a new name"),
            ));
        }
    }
    bind_node(scope, &pattern.start, path)?;
    for (rel, node) in &pattern.chain {
        if let Some(v) = &rel.var {
            if Some(v.as_str()) == path {
                return Err(already_bound_in_path(v));
            }
            let kind = if rel.hops.is_some() {
                Kind::VarRel
            } else {
                Kind::Rel
            };
            match scope.get(v) {
                None | Some(Kind::Unknown) => {
                    scope.vars.insert(v.clone(), kind);
                }
                Some(k) if k.is_relationship() => {}
                Some(k) => return Err(type_conflict(v, k, "a relationship")),
            }
        }
        bind_node(scope, node, path)?;
    }
    if let Some(p) = path {
        scope.vars.insert(p.to_string(), Kind::Path);
    }
    Ok(())
}

fn bind_node(scope: &mut Scope, node: &NodePat, path: Option<&str>) -> Result<(), String> {
    let Some(v) = &node.var else {
        return Ok(());
    };
    if Some(v.as_str()) == path {
        return Err(already_bound_in_path(v));
    }
    match scope.get(v) {
        None | Some(Kind::Unknown) => {
            scope.vars.insert(v.clone(), Kind::Node);
            Ok(())
        }
        Some(Kind::Node) => Ok(()),
        Some(k) => Err(type_conflict(v, k, "a node")),
    }
}

fn already_bound_in_path(v: &str) -> String {
    refuse(
        VARIABLE_ALREADY_BOUND,
        format!("`{v}` is already bound: it names the path and a part of it"),
    )
}

fn type_conflict(v: &str, was: Kind, now: &str) -> String {
    refuse(
        VARIABLE_TYPE_CONFLICT,
        format!("`{v}` is {} and cannot be used as {now}", was.describe()),
    )
}

/// The property maps and `shortestPath` parts of a pattern.
fn check_pattern_operands(scope: &Scope, pattern: &Pattern) -> Result<(), String> {
    let nodes = std::iter::once(&pattern.start).chain(pattern.chain.iter().map(|(_, n)| n));
    for node in nodes {
        for (_, value) in &node.props {
            check_operand(scope, value, Ctx::Value)?;
        }
    }
    Ok(())
}

fn check_unwind(scope: &Scope, list: &UnwindExpr) -> Result<(), String> {
    match list {
        UnwindExpr::Expr(op) => check_operand(scope, op, Ctx::Value),
        UnwindExpr::Prop { var, field } => check_property(scope, var, field),
        UnwindExpr::Lit(_) | UnwindExpr::Var(_) => Ok(()),
    }
}

/// Where an expression sits: a WHERE (no aggregate may appear) or any other
/// value position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Where,
    Value,
}

fn check_where(scope: &Scope, expr: &Expr) -> Result<(), String> {
    check_expr(scope, expr, Ctx::Where)
}

/// RETURN or WITH items.
fn check_items(scope: &Scope, items: &[RetItem]) -> Result<(), String> {
    for item in items {
        match &item.value {
            RetVal::Var(_) => {}
            RetVal::Prop { var, field } => check_property(scope, var, field)?,
            RetVal::Agg { func, arg } => {
                if let AggFunc::PercentileCont { p } | AggFunc::PercentileDisc { p } = func {
                    check_operand(scope, p, Ctx::Value)?;
                }
                if let AggArg::Prop { var, field } = arg {
                    check_property(scope, var, field)?;
                }
            }
            RetVal::FuncCall { name, args } => check_call(scope, name, args, Ctx::Value)?,
            RetVal::ScalarExpr(op) => check_operand(scope, op, Ctx::Value)?,
        }
    }
    Ok(())
}

/// ORDER BY: an aggregate is allowed only when the projection it sorts
/// aggregates too (it then sorts by that projected aggregate).
fn check_order_by(scope: &Scope, order: &[OrderItem], items: &[RetItem]) -> Result<(), String> {
    let aggregating = items
        .iter()
        .any(|item| matches!(item.value, RetVal::Agg { .. }));
    for item in order {
        match &item.target {
            OrderTarget::Alias(_) | OrderTarget::Var(_) => {}
            OrderTarget::Prop { var, field } => check_property(scope, var, field)?,
            OrderTarget::Expr(op) => {
                if !aggregating && contains_aggregate(op) {
                    return Err(refuse(
                        INVALID_AGGREGATION,
                        "ORDER BY cannot sort by an aggregate the projection does not compute",
                    ));
                }
                if !aggregating {
                    check_operand(scope, op, Ctx::Value)?;
                }
            }
        }
    }
    Ok(())
}

/// Whether an operand calls an aggregate function anywhere inside it.
fn contains_aggregate(op: &Operand) -> bool {
    let mut found = false;
    visit_operand(op, &mut |o| {
        if let Operand::FuncCall { name, .. } = o {
            if is_aggregate_name(name) {
                found = true;
            }
        }
    });
    found
}

/// Call `f` on `op` and every operand nested in it, predicates included.
fn visit_operand(op: &Operand, f: &mut dyn FnMut(&Operand)) {
    f(op);
    match op {
        Operand::Prop { .. } | Operand::Lit(_) | Operand::Param(_) | Operand::Var(_) => {}
        Operand::FuncCall { args, .. } | Operand::ListLit(args) => {
            for a in args {
                visit_operand(a, f);
            }
        }
        Operand::BinArith { left, right, .. } => {
            visit_operand(left, f);
            visit_operand(right, f);
        }
        Operand::Index { base, index } => {
            visit_operand(base, f);
            visit_operand(index, f);
        }
        Operand::ListComprehension {
            list, filter, map, ..
        } => {
            visit_operand(list, f);
            if let Some(e) = filter {
                visit_expr(e, f);
            }
            visit_operand(map, f);
        }
        Operand::ListPredicate { list, pred, .. } => {
            visit_operand(list, f);
            visit_expr(pred, f);
        }
        Operand::Reduce {
            init, list, body, ..
        } => {
            visit_operand(init, f);
            visit_operand(list, f);
            visit_operand(body, f);
        }
        Operand::MapLit(entries) => {
            for (_, v) in entries {
                visit_operand(v, f);
            }
        }
        Operand::Case { branches, default } => {
            for (cond, value) in branches {
                visit_expr(cond, f);
                visit_operand(value, f);
            }
            if let Some(d) = default {
                visit_operand(d, f);
            }
        }
        Operand::Pred(e) => visit_expr(e, f),
    }
}

fn visit_expr(expr: &Expr, f: &mut dyn FnMut(&Operand)) {
    match expr {
        Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) => {
            visit_expr(a, f);
            visit_expr(b, f);
        }
        Expr::Not(e) => visit_expr(e, f),
        Expr::Cmp { lhs, rhs, .. } => {
            visit_operand(lhs, f);
            visit_operand(rhs, f);
        }
        Expr::Truthy(o) | Expr::IsNull(o) | Expr::IsNotNull(o) => visit_operand(o, f),
        Expr::In { expr, list } => {
            visit_operand(expr, f);
            visit_operand(list, f);
        }
    }
}

fn check_expr(scope: &Scope, expr: &Expr, ctx: Ctx) -> Result<(), String> {
    match expr {
        Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) => {
            check_expr(scope, a, ctx)?;
            check_expr(scope, b, ctx)
        }
        Expr::Not(e) => check_expr(scope, e, ctx),
        Expr::Cmp { lhs, rhs, .. } => {
            check_operand(scope, lhs, ctx)?;
            check_operand(scope, rhs, ctx)
        }
        Expr::Truthy(o) | Expr::IsNull(o) | Expr::IsNotNull(o) => check_operand(scope, o, ctx),
        Expr::In { expr, list } => {
            check_operand(scope, expr, ctx)?;
            check_operand(scope, list, ctx)?;
            check_in_list(list)
        }
    }
}

/// `x IN <literal>`: a literal that is not a list (or null) is refused.
fn check_in_list(list: &Operand) -> Result<(), String> {
    let not_a_list = match list {
        Operand::Lit(Value::List(_) | Value::Null) => false,
        Operand::Lit(_) | Operand::MapLit(_) => true,
        _ => false,
    };
    if not_a_list {
        return Err(refuse(
            INVALID_ARGUMENT_TYPE,
            "IN needs a list on its right-hand side",
        ));
    }
    Ok(())
}

fn check_operand(scope: &Scope, op: &Operand, ctx: Ctx) -> Result<(), String> {
    match op {
        Operand::Lit(_) | Operand::Param(_) | Operand::Var(_) => Ok(()),
        Operand::Prop { var, field } => check_property(scope, var, field),
        Operand::FuncCall { name, args } => check_call(scope, name, args, ctx),
        Operand::BinArith { left, right, .. } => {
            check_operand(scope, left, ctx)?;
            check_operand(scope, right, ctx)
        }
        Operand::Index { base, index } => {
            check_operand(scope, base, ctx)?;
            check_operand(scope, index, ctx)
        }
        Operand::ListLit(items) => {
            for item in items {
                check_operand(scope, item, ctx)?;
            }
            Ok(())
        }
        Operand::MapLit(entries) => {
            for (_, value) in entries {
                check_operand(scope, value, ctx)?;
            }
            Ok(())
        }
        Operand::ListComprehension {
            var,
            list,
            filter,
            map,
        } => {
            check_operand(scope, list, ctx)?;
            let inner = scope.with_local(&[var]);
            if let Some(e) = filter {
                check_expr(&inner, e, ctx)?;
            }
            check_operand(&inner, map, ctx)
        }
        Operand::ListPredicate {
            var, list, pred, ..
        } => {
            check_operand(scope, list, ctx)?;
            check_expr(&scope.with_local(&[var]), pred, ctx)
        }
        Operand::Reduce {
            acc_var,
            init,
            iter_var,
            list,
            body,
        } => {
            check_operand(scope, init, ctx)?;
            check_operand(scope, list, ctx)?;
            check_operand(&scope.with_local(&[acc_var, iter_var]), body, ctx)
        }
        Operand::Case { branches, default } => {
            for (cond, value) in branches {
                check_expr(scope, cond, ctx)?;
                check_operand(scope, value, ctx)?;
            }
            match default {
                Some(d) => check_operand(scope, d, ctx),
                None => Ok(()),
            }
        }
        Operand::Pred(e) => check_expr(scope, e, ctx),
    }
}

/// `var.field`: a value with no properties, or a path's field other than
/// `length`, `nodes` and `relationships`, is refused.
fn check_property(scope: &Scope, var: &str, field: &str) -> Result<(), String> {
    match scope.get(var) {
        Some(Kind::Value(ValueType::NoProperties)) => Err(refuse(
            INVALID_ARGUMENT_TYPE,
            format!("`{var}` is a number, string, boolean or list and has no property `{field}`"),
        )),
        Some(Kind::Path) if !matches!(field, "length" | "nodes" | "relationships") => Err(refuse(
            INVALID_ARGUMENT_TYPE,
            format!(
                "`{var}` is a path; a path has `length`, `nodes` and `relationships`, \
                     not `{field}`"
            ),
        )),
        _ => Ok(()),
    }
}

/// A function call: an aggregate in a WHERE, an unknown name, or an argument
/// of a kind the function cannot take.
fn check_call(scope: &Scope, name: &str, args: &[Operand], ctx: Ctx) -> Result<(), String> {
    if name == LABEL_TEST_FN || name == REL_ENDS_FN {
        return Ok(());
    }
    check_argument_kinds(scope, name, args)?;
    if is_aggregate_name(name) {
        if ctx == Ctx::Where {
            return Err(refuse(
                INVALID_AGGREGATION,
                format!("`{name}()` is an aggregate and cannot be used in WHERE"),
            ));
        }
    } else if !super::exec::is_known_function(name) {
        return Err(refuse(
            UNKNOWN_FUNCTION,
            super::exec::unknown_function_message(name),
        ));
    }
    for arg in args {
        check_operand(scope, arg, ctx)?;
    }
    Ok(())
}

fn check_argument_kinds(scope: &Scope, name: &str, args: &[Operand]) -> Result<(), String> {
    let [arg] = args else {
        return Ok(());
    };
    let kind = operand_kind(scope, arg);
    let norm = name.to_ascii_lowercase();
    let refused = match norm.as_str() {
        "type" => matches!(kind, Kind::Node | Kind::Path),
        "length" => matches!(kind, Kind::Node | Kind::Rel),
        "size" => kind == Kind::Path,
        "properties" => kind == Kind::Value(ValueType::NoProperties),
        _ => false,
    };
    if refused {
        let what = match arg {
            Operand::Var(v) => format!("`{v}`, {}", kind.describe()),
            Operand::Lit(v) if v != &Value::Null => crate::value_ops::type_name_of(v).to_string(),
            Operand::ListLit(_) => "a list".to_string(),
            _ => kind.describe().to_string(),
        };
        return Err(refuse(
            INVALID_ARGUMENT_TYPE,
            format!("{name}() cannot take {what}"),
        ));
    }
    Ok(())
}

/// Check a write statement: the names its MATCH patterns bind, and the read
/// part of an `UNWIND`- or `MATCH`-led statement.
pub fn check_write(stmt: &WriteStatement) -> Result<(), String> {
    let patterns = match stmt {
        WriteStatement::Create(_) | WriteStatement::Merge(_) => return Ok(()),
        WriteStatement::MatchSet(s) => &s.matches,
        WriteStatement::MatchSetMap(s) => &s.matches,
        WriteStatement::MatchRemove(s) => &s.matches,
        WriteStatement::MatchDelete(s) => &s.matches,
        WriteStatement::MatchDeleteNode(s) => &s.matches,
        WriteStatement::UnwindWrite(s) => return check_query(&s.read),
    };
    bind_patterns(&mut Scope::default(), patterns)
}
