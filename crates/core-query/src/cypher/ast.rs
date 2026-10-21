//! Cypher subset AST. Types match the Plan 3 Task 6 interface block.

use crate::filter::CmpOp;
use core_storage::Value;

/// A LIMIT or SKIP value: either an exact count or a named query parameter.
///
/// `$name` parameters are resolved at execution time from the params map and
/// validated to be a non-negative integer.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitSkip {
    Exact(u64),
    Param(String),
}

/// One `OPTIONAL MATCH pattern [WHERE expr]` clause.
///
/// If the pattern produces no rows for a given input row, the input row
/// survives with the optional variables set to `null` (left-outer-join
/// semantics, openCypher §10.1.3).
///
/// `where_expr`, when present, is applied INSIDE the optional scope:
/// it filters candidate rows before the left-outer fallback fires.  This
/// differs from a post-filter that would eliminate the null row entirely.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionalClause {
    pub patterns: Vec<Pattern>,
    /// WHERE clause scoped to this optional match (applied before nullification).
    pub where_expr: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub matches: Vec<Pattern>,
    /// `OPTIONAL MATCH` clauses that follow the required matches.
    pub optional_clauses: Vec<OptionalClause>,
    /// Top-level WHERE filter, evaluated before UNWIND expansion.
    pub where_expr: Option<Expr>,
    /// Top-level UNWIND clauses (after WHERE, before post_unwind_where/WITH/RETURN).
    pub unwinds: Vec<UnwindClause>,
    /// Optional WHERE evaluated after UNWIND expansion (references UNWIND aliases).
    pub post_unwind_where: Option<Expr>,
    /// WITH pipeline stages. Each stage carries a WITH clause and optional
    /// MATCH / UNWIND / WHERE that follow it.
    pub stages: Vec<WithStage>,
    pub returns: Vec<RetItem>,
    /// `RETURN DISTINCT …` — executor hashes projected rows after `Project`.
    pub distinct: bool,
    pub order_by: Vec<OrderItem>,
    pub skip: Option<LimitSkip>,
    pub limit: Option<LimitSkip>,
}

/// One `UNWIND <expr> AS <alias>` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    pub list: UnwindExpr,
    pub alias: String,
}

/// The expression whose value is iterated in UNWIND.
#[derive(Debug, Clone, PartialEq)]
pub enum UnwindExpr {
    /// Inline list literal: `[1, 2, 3]`.
    Lit(Vec<Value>),
    /// Property on a bound node: `n.tags`.
    Prop { var: String, field: String },
    /// A previously bound alias (from a prior WITH): `alias`.
    Var(String),
    /// A list expression that is not a plain literal list: `[n.x, 1 + 1]`,
    /// `[{a: 1}, {a: 2}]`'s non-constant forms.
    Expr(Operand),
}

/// One WITH stage in a pipeline:
/// ```text
/// WITH <items> [WHERE <expr>] [ORDER BY …] [SKIP n] [LIMIT n]
/// [MATCH …]* [OPTIONAL MATCH …]* [UNWIND …]* [WHERE <expr>]
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct WithStage {
    /// The projected items in the WITH clause.
    pub items: Vec<RetItem>,
    /// Optional WHERE / HAVING filter immediately after the WITH keyword.
    pub where_expr: Option<Expr>,
    pub order_by: Vec<OrderItem>,
    pub skip: Option<LimitSkip>,
    pub limit: Option<LimitSkip>,
    /// MATCH clauses that follow this WITH.
    pub matches: Vec<Pattern>,
    /// OPTIONAL MATCH clauses that follow the required MATCHes in this stage.
    pub optional_clauses: Vec<OptionalClause>,
    /// UNWIND clauses that follow this WITH.
    pub unwinds: Vec<UnwindClause>,
    /// WHERE clause that follows those MATCHes (pre-next-WITH/RETURN filter).
    pub post_where: Option<Expr>,
}

/// Aggregate function in a RETURN clause.
#[derive(Debug, Clone, PartialEq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// `collect(x)` — gather each row's value of `x` into a list, skipping
    /// nulls. Per group when grouping keys are present.
    Collect,
}

/// Argument to an aggregate function.
#[derive(Debug, Clone, PartialEq)]
pub enum AggArg {
    /// `COUNT(*)` — every matched row counts regardless of binding.
    Star,
    /// `COUNT(var)` — counts rows where `var` is bound (non-null).
    Var(String),
    /// `SUM(var.field)`, `AVG(var.field)`, etc.
    Prop { var: String, field: String },
    /// `COUNT(DISTINCT var)` / `COLLECT(DISTINCT var.field)` — the inner
    /// argument is fed to the accumulator at most once per distinct value
    /// within a group.
    ///
    /// This is what makes an N-way relation intersection expressible: a
    /// company reached by three edge types yields three rows per talent, and
    /// only `count(DISTINCT t)` counts the talent once. The parser rejects
    /// `DISTINCT *` and nested `DISTINCT`, so the inner argument is always
    /// `Star`-free and one level deep.
    Distinct(Box<AggArg>),
}

/// Hop-count range for variable-length relationship patterns (`*min..max`).
/// Both bounds are inclusive.  `min = max` is a fixed-hop pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopRange {
    pub min: u8,
    pub max: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub start: NodePat,
    pub chain: Vec<(RelPat, NodePat)>,
    /// True when parsed as `MATCH shortestPath(...)`.
    pub shortest: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodePat {
    pub var: Option<String>,
    pub label: Option<String>,
    pub props: Vec<(String, Operand)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDir {
    Right,
    Left,
    Undirected,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelPat {
    pub var: Option<String>,
    /// Relationship-type alternatives. Empty = any type; one = single type;
    /// many = `[:A|:B]` alternation (match an edge of any listed type).
    pub etypes: Vec<String>,
    pub dir: RelDir,
    /// `None` = single-hop (normal `Expand`).  `Some(r)` = variable-length
    /// (`VarExpand`) with the given min/max hop bounds.
    pub hops: Option<HopRange>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    /// `a XOR b`: true when exactly one side is true.
    Xor(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Cmp {
        lhs: Operand,
        op: CmpOp,
        rhs: Operand,
    },
    /// Standalone operand used as a boolean predicate.
    ///
    /// Enables `WHERE textMatches(n.bio, 'query')` without requiring an
    /// explicit comparison.  Truthiness: `Bool(true)` → true, `Bool(false)`
    /// → false, null → false, any other non-null non-false value → true.
    Truthy(Operand),
    /// `operand IS NULL` — true iff the operand evaluates to null.
    IsNull(Operand),
    /// `operand IS NOT NULL` — true iff the operand is non-null.
    IsNotNull(Operand),
    /// `expr IN [a, b, $p]` or `expr IN $list` (`$list` is `Value::List`).
    In {
        expr: Operand,
        list: Vec<Operand>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Prop {
        var: String,
        field: String,
    },
    Lit(Value),
    Param(String),
    /// Bare variable reference (used in `WITH … WHERE alias > 2`).
    Var(String),
    /// Scalar function call: `toLower(n.name)`, `size(n.tags)`, `type(r)`, etc.
    ///
    /// Supported functions (case-insensitive): `toLower`, `toUpper`, `size`,
    /// `coalesce`, `type`, `abs`, `round`, `textMatches`, `contains`,
    /// `startsWith`, `endsWith`, `toInteger`, `toFloat`, `toString`, `decay`,
    /// `key`, `id`.
    /// Unknown names → named error at execution time listing the supported
    /// set (see `SCALAR_FUNCS` in `exec.rs`).
    FuncCall {
        name: String,
        args: Vec<Operand>,
    },
    /// Arithmetic expression inside a function argument: `abs(n.age - 27)`,
    /// `round(n.score * 1.5)`.  Supports `+`, `-`, `*`, `/`.
    BinArith {
        op: ArithOp,
        left: Box<Operand>,
        right: Box<Operand>,
    },
    /// List subscript: `n.tags[0]`, `n.location[1]`, `$list[$i]`.
    ///
    /// Evaluates `base`, which must be a `Value::List`, and returns the
    /// element at `index`. A negative index counts from the end
    /// (openCypher §3.4.4). An out-of-range index, a non-list base, or a
    /// non-integer index all evaluate to null rather than erroring, so a
    /// subscript behaves like a missing property.
    Index {
        base: Box<Operand>,
        index: Box<Operand>,
    },
    /// List literal with at least one non-constant element: `[n.x, 1 + 1]`.
    /// A list whose elements are all constants is folded to
    /// `Operand::Lit(Value::List(..))` at parse time. Evaluates to
    /// `Value::List`; an element that evaluates to null is an error because
    /// `Value` has no null variant.
    ListLit(Vec<Operand>),
    /// List comprehension: `[x IN list WHERE pred | expr]`.
    ///
    /// `var` is bound to each element of `list` inside `filter` and `map`
    /// only; it shadows an outer variable of the same name there. A null
    /// `list` (or a non-list) yields null. An element for which `filter` is
    /// false or null is dropped. `map` is `Operand::Var(var)` when the source
    /// has no `| expr` part. A null mapped value stays in the list as null.
    ListComprehension {
        var: String,
        list: Box<Operand>,
        filter: Option<Box<Expr>>,
        map: Box<Operand>,
    },
    /// `reduce(acc = init, x IN list | body)`.
    ///
    /// `init` and `list` are evaluated in the enclosing scope; `acc_var` and
    /// `iter_var` are bound inside `body` only and shadow outer names there.
    /// Each element of `list` is bound to `iter_var`, the accumulator to
    /// `acc_var`, and the value of `body` becomes the next accumulator. A null
    /// or non-list `list` yields null; a null `body` value makes the
    /// accumulator null for the next step.
    Reduce {
        acc_var: String,
        init: Box<Operand>,
        iter_var: String,
        list: Box<Operand>,
        body: Box<Operand>,
    },
    /// Map literal with at least one non-constant value: `{k: n.x}`. Keys are
    /// unique (a duplicate is a parse error). All-constant maps fold to
    /// `Operand::Lit(Value::Map(..))`. Evaluates to `Value::Map`.
    MapLit(Vec<(String, Operand)>),
    /// Generic `CASE WHEN <cond> THEN <value> [WHEN …] [ELSE <value>] END`.
    /// Evaluates each branch's condition in order, returning the first matching
    /// value; the `default` (ELSE) or null if none match.
    Case {
        branches: Vec<(Expr, Operand)>,
        default: Option<Box<Operand>>,
    },
    /// A predicate in a value position: `RETURN n.age > 18 AS adult`,
    /// `[x IN l | x > 1]`, `SET n.flag = NOT n.done`. Evaluates three-valued
    /// to `Bool(true)`, `Bool(false)` or null: a comparison or `IN` with a
    /// null side is null, `NOT null` is null, `null AND false` is false and
    /// `null OR true` is true. A WHERE never holds one at its top: the parser
    /// unwraps it to the `Expr` itself, which WHERE evaluates as before.
    Pred(Box<Expr>),
}

/// The column name an operand gets in a RETURN or WITH item that has no
/// `AS` alias.
///
/// There are three copies of the column-naming rule — the planner's
/// duplicate-column check, the executor's projection, and core-api's
/// write-statement RETURN — and they have to agree exactly or a `WITH` alias
/// stops resolving. This is the one place the operand half of that rule
/// lives.
///
/// A composite operand renders as a placeholder (`<arith>`, `<case>`)
/// because it has no natural spelling; a subscript does have one, so
/// `n.location[0]` names itself and two subscripts of the same list are two
/// distinct columns rather than a duplicate-column error.
pub fn operand_label(op: &Operand) -> String {
    match op {
        Operand::Var(v) => v.clone(),
        Operand::Prop { var, field } => format!("{var}.{field}"),
        Operand::Lit(_) => "<lit>".to_string(),
        Operand::Param(p) => format!("${p}"),
        Operand::FuncCall { name, .. } => format!("{name}(...)"),
        Operand::BinArith { .. } => "<arith>".to_string(),
        Operand::Case { .. } => "<case>".to_string(),
        Operand::ListLit(_) => "<list>".to_string(),
        Operand::MapLit(_) => "<map>".to_string(),
        Operand::ListComprehension { .. } => "<listcomp>".to_string(),
        Operand::Reduce { .. } => "<reduce>".to_string(),
        Operand::Pred(_) => "<pred>".to_string(),
        Operand::Index { base, index } => {
            format!("{}[{}]", operand_label(base), subscript_label(index))
        }
    }
}

/// The index half of a subscript's column name. A literal integer prints as
/// itself — `<lit>` would make every subscript of one list the same column.
fn subscript_label(op: &Operand) -> String {
    match op {
        Operand::Lit(Value::Int(n)) => n.to_string(),
        other => operand_label(other),
    }
}

/// The column name a RETURN item takes when it has no `AS` alias, for the
/// item kinds whose name does not depend on the planner.
///
/// `RetVal::Agg` is not here: an aggregate's column is computed by the
/// planner and stored on the plan op, and the three callers disagree about
/// what to do with it.
pub fn ret_val_label(value: &RetVal) -> Option<String> {
    match value {
        RetVal::Var(v) => Some(v.clone()),
        RetVal::Prop { var, field } => Some(format!("{var}.{field}")),
        RetVal::FuncCall { name, args } => {
            let arg_strs: Vec<String> = args.iter().map(operand_label).collect();
            Some(format!("{name}({})", arg_strs.join(", ")))
        }
        // A subscript names itself; every other scalar expression is `<expr>`.
        RetVal::ScalarExpr(op @ Operand::Index { .. }) => Some(operand_label(op)),
        RetVal::ScalarExpr(_) => Some("<expr>".to_string()),
        RetVal::Agg { .. } => None,
    }
}

/// Arithmetic operators for `Operand::BinArith`.
#[derive(Debug, Clone, PartialEq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

/// RETURN item value: bare variable, `var.field`, an aggregate call, a
/// scalar function call, or an arbitrary scalar expression (arithmetic etc.).
#[derive(Debug, Clone, PartialEq)]
pub enum RetVal {
    Var(String),
    Prop {
        var: String,
        field: String,
    },
    /// Single aggregate function call.  When combined with non-aggregate items
    /// in the same RETURN clause, the planner routes to `GroupAggregate`.
    /// Multiple aggregate items are also supported via `GroupAggregate`.
    Agg {
        func: AggFunc,
        arg: AggArg,
    },
    /// Scalar function call in a RETURN position, e.g. `toLower(n.name)`.
    /// The same function set as `Operand::FuncCall` (see `SCALAR_FUNCS`).
    FuncCall {
        name: String,
        args: Vec<Operand>,
    },
    /// Arbitrary scalar expression in a RETURN/WITH position, e.g. `n.age + 1`.
    /// Evaluated via `resolve_operand` at execution time; null propagates.
    ScalarExpr(Operand),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetItem {
    pub value: RetVal,
    pub alias: Option<String>,
}

/// ORDER BY target. Bare identifiers that match a RETURN alias become `Alias`;
/// otherwise they stay `Var`. `var.field` is always `Prop`. Anything else — an
/// arithmetic expression, a function call, a subscript, a `CASE` — is `Expr`.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderTarget {
    Alias(String),
    Var(String),
    Prop { var: String, field: String },
    Expr(Operand),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub target: OrderTarget,
    pub descending: bool,
}

// ─── Write statement AST ──────────────────────────────────────────────────────

/// Top-level write statement (CREATE / MATCH…SET / MATCH…DELETE / MATCH…DETACH DELETE / MERGE).
/// Produced by `parse_write`; executed by `GraphDb::query_write`.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteStatement {
    Create(CreateStmt),
    MatchSet(MatchSetStmt),
    MatchSetMap(MatchSetMapStmt),
    MatchRemove(MatchRemoveStmt),
    MatchDelete(MatchDeleteStmt),
    MatchDeleteNode(MatchDeleteNodeStmt),
    Merge(MergeStmt),
    /// `UNWIND … [MATCH|WITH …]* <write clause>+ [RETURN …]`: the write clauses
    /// run once per row the read part produces.
    UnwindWrite(Box<UnwindWriteStmt>),
}

/// `CREATE (a:L {id: 'x', ...})[-[:T]->(b:L2 {id: 'y', ...})] [RETURN …]`
///
/// `nodes` is in encounter order. `edges` reference node vars that appear in
/// `nodes`. Each node must have a string-valued `id` property (used as key).
///
/// `returns`, when `Some`, projects the created bindings as a read result.
/// The write and the projection are committed as a single WAL batch frame.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateStmt {
    pub nodes: Vec<CreateNode>,
    pub edges: Vec<CreateEdge>,
    /// Optional RETURN clause: project created bindings after commit.
    pub returns: Option<Vec<RetItem>>,
}

/// One node in a CREATE pattern.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateNode {
    /// Optional binding variable (`a` in `(a:Label {…})`).
    pub var: Option<String>,
    pub label: String,
    /// Literal property pairs.  Must include a string-valued `id` field.
    pub props: Vec<(String, core_storage::Value)>,
}

/// One edge in a CREATE pattern, referencing vars from `CreateStmt.nodes`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateEdge {
    pub src_var: String,
    pub etype: String,
    pub dst_var: String,
}

/// `MATCH patterns [WHERE expr] SET var.field = literal [, …] [RETURN …]`
#[derive(Debug, Clone, PartialEq)]
pub struct MatchSetStmt {
    pub matches: Vec<Pattern>,
    pub where_expr: Option<Expr>,
    pub sets: Vec<SetClause>,
    /// Optional RETURN clause: project matched bindings from post-write state.
    pub returns: Option<Vec<RetItem>>,
}

/// `MATCH patterns [WHERE expr] SET var = {…} | SET var += {…} [RETURN …]`
///
/// `merge` is `true` for `+=` (set the listed keys, leave every other property,
/// a null value removes that property) and `false` for `=` (the node's user
/// properties become exactly the map; the key `id`, the namespace `ns` and view
/// owned properties are never removed, and a null value is an absent key).
/// `var` is a node variable bound by `matches` (the parser checks).
#[derive(Debug, Clone, PartialEq)]
pub struct MatchSetMapStmt {
    pub matches: Vec<Pattern>,
    pub where_expr: Option<Expr>,
    pub merge: bool,
    pub var: String,
    pub map: Vec<(String, Operand)>,
    /// `SET n = $param` / `SET n += $param`: the parameter holding the map, with
    /// `map` empty. The parameter must be a `Value::Map` at execution time.
    pub param: Option<String>,
    /// Optional RETURN clause: project matched bindings from post-write state.
    pub returns: Option<Vec<RetItem>>,
}

/// `MATCH patterns [WHERE expr] REMOVE var.field [, …] [RETURN …]`
///
/// Every `var` is a node variable bound by `matches` (the parser checks).
/// Removing a property a node does not have is a no-op, not an error.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchRemoveStmt {
    pub matches: Vec<Pattern>,
    pub where_expr: Option<Expr>,
    /// `(var, field)` in source order.
    pub removals: Vec<(String, String)>,
    /// Optional RETURN clause: project matched bindings from post-write state.
    pub returns: Option<Vec<RetItem>>,
}

/// One `var.field = literal_or_param` assignment in a SET clause.
///
/// `value` is an `Operand` rather than a bare `Value` so that `$param`
/// references are legal on the RHS (resolved at execution time from the
/// query's parameter map).  Only `Operand::Lit` and `Operand::Param` are
/// accepted by the parser; other variants produce a named parse error.
#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    pub var: String,
    pub field: String,
    pub value: Operand,
}

/// `MATCH patterns [WHERE expr] DELETE rel_var [, …]`
///
/// Each `EdgeDelete` carries the etype, src node var, and dst node var resolved
/// from the MATCH patterns at parse time so the executor can emit a read query
/// returning node keys without extra pattern-scanning.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchDeleteStmt {
    pub matches: Vec<Pattern>,
    pub where_expr: Option<Expr>,
    pub deletes: Vec<EdgeDelete>,
}

/// One edge variable to delete, with resolved topology.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeDelete {
    pub rel_var: String,
    pub etype: String,
    pub src_var: String,
    pub dst_var: String,
}

/// `MATCH patterns [WHERE expr] [DETACH] DELETE node_var [, …]`
///
/// When `detach` is `true` (DETACH DELETE), all incident edges are removed
/// before the node is tombstoned (openCypher semantics).  When `false`
/// (bare DELETE), the node must have no incident edges; the executor returns
/// an error if any remain.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchDeleteNodeStmt {
    pub matches: Vec<Pattern>,
    pub where_expr: Option<Expr>,
    /// Node variable names whose nodes are to be deleted (resolved from MATCH patterns).
    pub node_vars: Vec<String>,
    /// `true` → DETACH DELETE (allowed regardless of edges).
    /// `false` → bare DELETE (error if any edges touch the node).
    pub detach: bool,
}

/// `MERGE (n:Label {id: 'x'}) [ON CREATE SET …] [ON MATCH SET …] [RETURN …]`
///
/// Exactly one identifying property is allowed (the key). An optional `ns`
/// names the namespace the create arm writes into; any other extra property
/// is a named error. `ON CREATE SET` / `ON MATCH SET` apply inside the same
/// write batch as the insert-or-skip. `returns`, when `Some`, projects the
/// node after commit.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeStmt {
    pub label: String,
    /// The single property that identifies the node. Value must be a string.
    pub key_field: String,
    pub key_value: core_storage::Value,
    /// Optional `ns` named in the MERGE pattern, distinct from the identifying
    /// key. Absent = the create arm does not name a namespace.
    pub ns: Option<core_storage::Value>,
    /// Optional bound variable for the MERGE node (for RETURN projection).
    pub var: Option<String>,
    /// `ON CREATE SET` assignments, applied only when the node is inserted.
    pub on_create: Vec<SetClause>,
    /// `ON MATCH SET` assignments, applied only when the node already exists.
    pub on_match: Vec<SetClause>,
    /// Optional RETURN clause: project the node after commit.
    pub returns: Option<Vec<RetItem>>,
    /// `Some` for `MERGE (a:L {id: 'x'})-[:T]->(b:M {id: 'y'})`: the one
    /// relationship and its end node. The fields above describe the start node.
    pub edge: Option<MergeEdgePat>,
}

/// The relationship and end node of a `MERGE (a)-[:T]->(b)` pattern. Exactly
/// one relationship is allowed in 0.10; the start node is the `MergeStmt`'s own.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeEdgePat {
    /// The single relationship type.
    pub etype: String,
    /// `Right`: start -> end. `Left`: end -> start. Undirected is refused at parse time.
    pub dir: RelDir,
    pub end_label: String,
    /// The end node's single identifying property.
    pub end_key_field: String,
    pub end_key_value: core_storage::Value,
    /// Optional `ns` named on the end node, as `MergeStmt::ns` is for the start.
    pub end_ns: Option<core_storage::Value>,
    /// Bound variable of the end node (`ON … SET b.x = …`, `RETURN b`).
    pub end_var: Option<String>,
    /// Bound variable of the relationship (`RETURN r`); it takes no SET.
    pub rel_var: Option<String>,
}

// ─── UNWIND-led write statements (QL-09b) ─────────────────────────────────────

/// `UNWIND <expr> AS x [MATCH … | WITH … | UNWIND …]* <write clause>+ [RETURN …]`
///
/// The `read` part (everything before the first write clause) produces one row
/// per iteration; each write clause then runs for each row, in order, and all
/// of it commits as one batch. A read clause after a write clause is refused at
/// parse time, so every read sees the state before the statement.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindWriteStmt {
    /// The read part. Its `returns` are empty: the executor projects what the
    /// write clauses need.
    pub read: Query,
    /// Names in scope at the end of the read part that hold a node or a value
    /// (not a relationship); a `RETURN` after the writes is evaluated with them.
    pub scope: Vec<String>,
    /// Every name that holds a node at the end of the statement: the nodes of
    /// the read part and the ones the write clauses create or merge.
    pub node_vars: Vec<String>,
    /// Every name that holds a relationship at the end of the statement: the
    /// read part's, and the ones a `MERGE` pattern binds.
    pub rel_vars: Vec<String>,
    /// The write clauses, in source order.
    pub writes: Vec<RowWrite>,
    /// Optional `RETURN [DISTINCT] … [ORDER BY …] [SKIP …] [LIMIT …]`.
    pub returns: Option<RowReturn>,
}

/// The RETURN clause of an UNWIND-led write statement.
#[derive(Debug, Clone, PartialEq)]
pub struct RowReturn {
    pub items: Vec<RetItem>,
    pub distinct: bool,
    pub order_by: Vec<OrderItem>,
    pub skip: Option<LimitSkip>,
    pub limit: Option<LimitSkip>,
}

/// One write clause of an UNWIND-led statement. Every `Operand` is evaluated
/// per row by the read part, so it may name the UNWIND alias, a matched node's
/// property, a `$parameter` or any expression over them.
#[derive(Debug, Clone, PartialEq)]
pub enum RowWrite {
    /// `CREATE (a:L {…})[-[:T]->(b:L {…})]`
    Create {
        nodes: Vec<RowCreateNode>,
        edges: Vec<CreateEdge>,
    },
    /// `MERGE (a:L {key: …}) [-[:T]->(b:L {key: …})] [ON CREATE SET …] [ON MATCH SET …]`
    Merge(Box<RowMerge>),
    /// `SET n.f = expr [, …]`
    Set(Vec<SetClause>),
    /// `SET n = <map>` / `SET n += <map>`; `value` evaluates to a map per row.
    SetMap {
        var: String,
        merge: bool,
        value: Operand,
    },
    /// `REMOVE n.f [, …]`
    Remove(Vec<(String, String)>),
    /// `[DETACH] DELETE n [, …]` (node variables only)
    Delete { vars: Vec<String>, detach: bool },
}

/// One node of a per-row CREATE. `var` is always set (the parser names an
/// anonymous node).
#[derive(Debug, Clone, PartialEq)]
pub struct RowCreateNode {
    pub var: String,
    pub label: String,
    pub props: Vec<(String, Operand)>,
}

/// A per-row MERGE: one node, or two nodes and the relationship between them.
#[derive(Debug, Clone, PartialEq)]
pub struct RowMerge {
    pub start: RowMergeNode,
    pub edge: Option<RowMergeEdge>,
    pub on_create: Vec<SetClause>,
    pub on_match: Vec<SetClause>,
}

/// A MERGE node pattern: its single identifying property and optional `ns`.
#[derive(Debug, Clone, PartialEq)]
pub struct RowMergeNode {
    /// Always set (the parser names an anonymous node).
    pub var: String,
    pub label: String,
    pub key_field: String,
    pub key: Operand,
    pub ns: Option<Operand>,
}

/// The relationship and end node of a per-row `MERGE (a)-[:T]->(b)`.
#[derive(Debug, Clone, PartialEq)]
pub struct RowMergeEdge {
    pub etype: String,
    pub dir: RelDir,
    pub rel_var: Option<String>,
    pub end: RowMergeNode,
}
