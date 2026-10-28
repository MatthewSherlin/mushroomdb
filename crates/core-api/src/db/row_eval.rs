//! Evaluating a write's values row by row, as the statement goes.
//!
//! A `SET n.count = n.count + 1` over several rows must read what the rows
//! before it wrote, and a `SET n.w = n.v` after `SET n.v = 10` must read 10. The
//! read part of a statement runs once, over the state before it, so a value that
//! reads a node's property cannot be taken from that result. Such a value is
//! evaluated here, at the moment its row is written, with every read of a node
//! property answered by [`Overlay`], the statement's own effects so far, before
//! the store. Everything else in the value (parameters, row variables,
//! functions, arithmetic) goes through the executor's own evaluator, so a
//! function means here what it means in a `RETURN`.
//!
//! The overlay is hash-keyed, so a pass over the rows is linear.
//!
//! Below the overlay is a [`RowSource`]: the store, for an auto-commit
//! statement, or a transaction's base snapshot with its buffered writes laid
//! over it, inside a transaction. The same pass runs over either.

use super::*;
use core_query::cypher::{eval_operand, Binding};

/// What a statement's per-row writes read beneath their own [`Overlay`].
///
/// [`GraphDb`] is one (an auto-commit statement reads the store, masked under
/// a role-scoped write); a transaction's state is the other (its base snapshot
/// with its earlier statements' writes laid over it). Every per-row write form
/// and every `MATCH … SET` / `REMOVE` form computes its batch ops through this
/// trait, so a statement inside a transaction makes the decisions it makes
/// auto-commit.
pub(crate) trait RowSource {
    /// Run `f` over the view the statement's read part runs in and its values
    /// are evaluated in.
    fn row_view<R>(&self, f: impl FnOnce(&GraphView<'_>) -> R) -> R;
    /// Whether the node `key` is live.
    fn row_exists(&self, key: &str) -> bool;
    /// The node's id in [`Self::row_view`]'s view, if it is live.
    fn row_id(&self, key: &str) -> Option<u32>;
    /// Every stored property of the node; empty when it is not live.
    fn row_props(&self, key: &str) -> BTreeMap<String, Value>;
    /// One stored property of the node.
    fn row_prop(&self, key: &str, field: &str) -> Option<Value>;
    /// The node's label.
    fn row_label(&self, key: &str) -> Option<String>;
    /// Whether the manual-or-derived edge `src -[etype]-> dst` is present.
    fn row_edge(&self, etype: &str, src: &str, dst: &str) -> bool;
    /// Whether any edge touches the node.
    fn row_has_edges(&self, key: &str) -> bool;
    /// The other end of each distinct edge that touches the node (the node
    /// itself for a self-loop, once).
    fn row_incident(&self, key: &str) -> Vec<String>;
    /// Whether a MERGE node exists, after any role-scope check.
    fn row_merge_existed(&self, label: &str, key: &str) -> Result<bool>;
    /// The properties a MERGE create arm inserts.
    fn row_merge_props(
        &self,
        key_field: &str,
        key_value: &Value,
        named_ns: Option<&Value>,
    ) -> Result<Vec<(String, Value)>>;
    /// Scope-before-lookup for a MERGE pattern of `label`: `Err` when a
    /// role-scoped write may neither create nor update that label.
    fn row_merge_scope(&self, label: &str) -> Result<()>;
    /// The namespace a MERGE create arm stamps when its pattern names none.
    fn row_merge_ns(&self) -> Result<Option<Value>>;
    /// Whether a view owns the property `field`.
    fn row_view_owns(&self, field: &str) -> bool;
    /// Every stored property of the edge `src -[etype]-> dst`; empty when it
    /// has none or is not there.
    fn row_edge_props(&self, etype: &str, src: &str, dst: &str) -> BTreeMap<String, Value>;
    /// The edge's type symbol and end ids in [`Self::row_view`]'s view, when
    /// its type and both ends are there.
    fn row_edge_ids(&self, etype: &str, src: &str, dst: &str) -> Option<(u32, u32, u32)>;
    /// The rule that derives edges of `etype`, if one does: such a type takes
    /// no user property write.
    fn row_rule_owning(&self, etype: &str) -> Option<String>;
}

impl<F: Fs> RowSource for GraphDb<F> {
    fn row_view<R>(&self, f: impl FnOnce(&GraphView<'_>) -> R) -> R {
        match self.pending_write_authz.as_ref() {
            Some(authz) => f(&self.view_masked(&authz.mask)),
            None => f(&self.view()),
        }
    }

    fn row_exists(&self, key: &str) -> bool {
        self.has_node(key)
    }

    fn row_id(&self, key: &str) -> Option<u32> {
        self.ids.get(key)
    }

    fn row_props(&self, key: &str) -> BTreeMap<String, Value> {
        self.node_info(key).map(|n| n.props).unwrap_or_default()
    }

    fn row_prop(&self, key: &str, field: &str) -> Option<Value> {
        self.get_prop(key, field)
    }

    fn row_label(&self, key: &str) -> Option<String> {
        self.node_ref(key).map(|n| n.label().to_owned())
    }

    fn row_edge(&self, etype: &str, src: &str, dst: &str) -> bool {
        self.merge_edge_present(etype, src, dst)
    }

    fn row_has_edges(&self, key: &str) -> bool {
        let Some(id) = self.ids.get(key) else {
            return false;
        };
        let tv = self.topo_view();
        tv.etypes().any(|et| {
            !tv.neighbors(et, Direction::Out, id).is_empty()
                || !tv.neighbors(et, Direction::In, id).is_empty()
        })
    }

    fn row_incident(&self, key: &str) -> Vec<String> {
        let Some(id) = self.ids.get(key) else {
            return Vec::new();
        };
        let tv = self.topo_view();
        let mut others = Vec::new();
        for et in tv.etypes() {
            let outs = tv.neighbors(et, Direction::Out, id);
            let ins = tv.neighbors(et, Direction::In, id);
            for &other in outs.iter().chain(ins.iter().filter(|&&src| src != id)) {
                if let Some(k) = self.ids.key_of(other) {
                    others.push(k.to_string());
                }
            }
        }
        others
    }

    fn row_merge_existed(&self, label: &str, key: &str) -> Result<bool> {
        self.merge_node_existed(label, key)
    }

    fn row_merge_props(
        &self,
        key_field: &str,
        key_value: &Value,
        named_ns: Option<&Value>,
    ) -> Result<Vec<(String, Value)>> {
        self.merge_create_props(key_field, key_value, named_ns)
    }

    fn row_merge_scope(&self, label: &str) -> Result<()> {
        self.merge_label_scope(label)
    }

    fn row_merge_ns(&self) -> Result<Option<Value>> {
        Ok(self.merge_create_stamp_ns()?.map(Value::Str))
    }

    fn row_view_owns(&self, field: &str) -> bool {
        self.view_store.view_for_prop(field).is_some()
    }

    fn row_edge_props(&self, etype: &str, src: &str, dst: &str) -> BTreeMap<String, Value> {
        match self.row_edge_ids(etype, src, dst) {
            Some((et, s, d)) => self.edge_props_view().props_of(et, s, d),
            None => BTreeMap::new(),
        }
    }

    fn row_edge_ids(&self, etype: &str, src: &str, dst: &str) -> Option<(u32, u32, u32)> {
        Some((
            self.syms.get(etype)?,
            self.ids.get(src)?,
            self.ids.get(dst)?,
        ))
    }

    fn row_rule_owning(&self, etype: &str) -> Option<String> {
        self.rule_owning_edge_type(etype)
    }
}

/// What the statement has done to one node so far.
pub(super) struct NodeState {
    /// The node exists at this point of the statement.
    pub(super) exists: bool,
    /// The statement created the node (its edges are not in the store).
    pub(super) created_here: bool,
    /// The node's properties at this point, once known.
    pub(super) props: Option<BTreeMap<String, Value>>,
    /// Writes made before `props` was loaded from the store: a value, or
    /// `None` for a removal. Applied over the stored properties when they load.
    pub(super) delta: BTreeMap<String, Option<Value>>,
    /// The label of a node the statement created (empty for none); `None`
    /// for a stored node, whose label the store holds.
    pub(super) label: Option<String>,
}

impl NodeState {
    pub(super) fn stored() -> Self {
        NodeState {
            exists: true,
            created_here: false,
            props: None,
            delta: BTreeMap::new(),
            label: None,
        }
    }

    /// The statement wrote the node's properties (so the store's may be stale).
    pub(super) fn touched(&self) -> bool {
        self.props.is_some() || !self.delta.is_empty()
    }
}

pub(super) type EdgeKey = (String, String, String);

/// What the statement has done to one relationship's properties so far.
pub(super) struct EdgeState {
    /// The properties at this point, once known.
    props: Option<BTreeMap<String, Value>>,
    /// Writes made before `props` was loaded: a value, or `None` for a removal.
    delta: BTreeMap<String, Option<Value>>,
}

/// The statement's own effects, read back by later rows.
#[derive(Default)]
pub(super) struct Overlay {
    pub(super) nodes: HashMap<String, NodeState>,
    /// Relationship properties the statement wrote (QL-RP).
    edge_props: HashMap<EdgeKey, EdgeState>,
    edges: HashSet<EdgeKey>,
    /// The edges the statement created, by each endpoint. An entry can name an
    /// edge that is gone from `edges` (its other end was deleted).
    touching: HashMap<String, Vec<EdgeKey>>,
}

impl Overlay {
    pub(super) fn node_created(&mut self, key: &str, label: &str, props: &[(String, Value)]) {
        self.nodes.insert(
            key.to_string(),
            NodeState {
                exists: true,
                created_here: true,
                props: Some(props.iter().cloned().collect()),
                delta: BTreeMap::new(),
                label: Some(label.to_string()),
            },
        );
    }

    pub(super) fn note_set(&mut self, key: &str, field: &str, value: &Value) {
        let state = self
            .nodes
            .entry(key.to_string())
            .or_insert_with(NodeState::stored);
        match state.props.as_mut() {
            Some(props) => {
                props.insert(field.to_string(), value.clone());
            }
            None => {
                state.delta.insert(field.to_string(), Some(value.clone()));
            }
        }
    }

    pub(super) fn note_remove(&mut self, key: &str, field: &str) {
        let state = self
            .nodes
            .entry(key.to_string())
            .or_insert_with(NodeState::stored);
        match state.props.as_mut() {
            Some(props) => {
                props.remove(field);
            }
            None => {
                state.delta.insert(field.to_string(), None);
            }
        }
    }

    pub(super) fn edge_created(&mut self, etype: &str, src: &str, dst: &str) {
        let edge: EdgeKey = (etype.to_string(), src.to_string(), dst.to_string());
        if self.edges.insert(edge.clone()) {
            self.touching
                .entry(src.to_string())
                .or_default()
                .push(edge.clone());
            if src != dst {
                self.touching.entry(dst.to_string()).or_default().push(edge);
            }
        }
    }

    /// A relationship the statement created: its properties are `props` and
    /// nothing the store held for the pair before.
    pub(super) fn edge_props_fresh(&mut self, edge: &EdgeKey, props: BTreeMap<String, Value>) {
        self.edge_props.insert(
            edge.clone(),
            EdgeState {
                props: Some(props),
                delta: BTreeMap::new(),
            },
        );
    }

    /// Record a relationship-property write: a value, or `None` for a removal.
    pub(super) fn note_edge_write(&mut self, edge: &EdgeKey, field: &str, value: Option<&Value>) {
        let state = self
            .edge_props
            .entry(edge.clone())
            .or_insert_with(|| EdgeState {
                props: None,
                delta: BTreeMap::new(),
            });
        match state.props.as_mut() {
            Some(props) => match value {
                Some(v) => {
                    props.insert(field.to_string(), v.clone());
                }
                None => {
                    props.remove(field);
                }
            },
            None => {
                state.delta.insert(field.to_string(), value.cloned());
            }
        }
    }

    pub(super) fn node_deleted(&mut self, key: &str) {
        for edge in self.touching.remove(key).unwrap_or_default() {
            self.edges.remove(&edge);
        }
        self.nodes.insert(
            key.to_string(),
            NodeState {
                exists: false,
                created_here: false,
                props: None,
                delta: BTreeMap::new(),
                label: None,
            },
        );
    }

    /// Edges the statement created at `key` that are still there.
    fn pending_edges_at(&self, key: &str) -> usize {
        self.touching.get(key).map_or(0, |edges| {
            edges.iter().filter(|e| self.edges.contains(*e)).count()
        })
    }

    fn has_pending_edge(&self, key: &str) -> bool {
        self.touching
            .get(key)
            .is_some_and(|edges| edges.iter().any(|e| self.edges.contains(e)))
    }
}

/// The label of `key` at this point of the statement (empty for none).
pub(super) fn uw_label<S: RowSource + ?Sized>(src: &S, ov: &Overlay, key: &str) -> Option<String> {
    match ov.nodes.get(key) {
        Some(state) if !state.exists => None,
        Some(NodeState {
            label: Some(label), ..
        }) => Some(label.clone()),
        _ => src.row_label(key),
    }
}

/// Whether `props` all hold on `key` at this point of the statement.
fn uw_props_hold<S: RowSource + ?Sized>(
    src: &S,
    ov: &mut Overlay,
    key: &str,
    props: &[(String, Value)],
) -> bool {
    let have = uw_props(src, ov, key);
    props
        .iter()
        .all(|(f, want)| have.get(f).is_some_and(|got| values_equal(got, want)))
}

/// The nodes a MERGE pattern that is not a key matches at this point of the
/// statement (QL-NM): every live node carrying `label` (any node when it is
/// empty) whose properties equal `props`, the statement's own writes
/// included. Stored nodes come in dense-id order, then the ones the
/// statement created or changed, by key. An equality index on the label
/// answers the scan when one covers a property.
pub(super) fn uw_pattern_matches<S: RowSource + ?Sized>(
    src: &S,
    ov: &mut Overlay,
    label: &str,
    props: &[(String, Value)],
) -> Vec<String> {
    let stored: Vec<String> = src.row_view(|view| {
        let indexed = (!label.is_empty())
            .then(|| {
                props
                    .iter()
                    .find_map(|(f, v)| view.nodes_with_prop(label, f, v))
            })
            .flatten();
        let ids: Vec<u32> = match indexed {
            Some(ids) => ids,
            None if label.is_empty() => view.nodes_all(),
            None => view
                .nodes_with_label(label)
                .into_iter()
                .filter(|&id| view.visible(id))
                .collect(),
        };
        ids.into_iter()
            .filter(|&id| {
                props.iter().all(|(f, want)| {
                    view.prop(id, f)
                        .is_some_and(|got| values_equal(&got.into_value(), want))
                })
            })
            .filter_map(|id| view.try_key_of(id).map(str::to_string))
            .collect()
    });
    let stored_set: HashSet<&str> = stored.iter().map(String::as_str).collect();
    let mut out: Vec<String> = Vec::new();
    let mut recheck: Vec<String> = Vec::new();
    for key in &stored {
        match ov.nodes.get(key.as_str()) {
            Some(state) if !state.exists => {}
            Some(state) if state.touched() => recheck.push(key.clone()),
            _ => out.push(key.clone()),
        }
    }
    // Nodes the statement created or changed that the store's scan could not
    // answer for.
    let mut extra: Vec<String> = ov
        .nodes
        .iter()
        .filter(|(k, state)| {
            state.exists
                && (state.created_here || state.touched())
                && !stored_set.contains(k.as_str())
        })
        .map(|(k, _)| k.clone())
        .collect();
    extra.sort();
    for key in recheck {
        if uw_props_hold(src, ov, &key, props) {
            out.push(key);
        }
    }
    for key in extra {
        let label_ok = match uw_label(src, ov, &key) {
            Some(l) => label.is_empty() || l == label,
            None => false,
        };
        if label_ok && uw_props_hold(src, ov, &key, props) {
            out.push(key);
        }
    }
    out
}

/// Whether the node `key` exists at this point of the statement.
pub(super) fn uw_exists<S: RowSource + ?Sized>(src: &S, ov: &Overlay, key: &str) -> bool {
    match ov.nodes.get(key) {
        Some(state) => state.exists,
        None => src.row_exists(key),
    }
}

/// The properties of `key` at this point of the statement.
pub(super) fn uw_props<'o, S: RowSource + ?Sized>(
    src: &S,
    ov: &'o mut Overlay,
    key: &str,
) -> &'o mut BTreeMap<String, Value> {
    let state = ov
        .nodes
        .entry(key.to_string())
        .or_insert_with(NodeState::stored);
    let delta = std::mem::take(&mut state.delta);
    state.props.get_or_insert_with(|| {
        let mut props = src.row_props(key);
        for (field, value) in delta {
            match value {
                Some(v) => props.insert(field, v),
                None => props.remove(&field),
            };
        }
        props
    })
}

/// The properties of the relationship `edge` at this point of the statement.
pub(super) fn uw_edge_props<'o, S: RowSource + ?Sized>(
    src: &S,
    ov: &'o mut Overlay,
    edge: &EdgeKey,
) -> &'o mut BTreeMap<String, Value> {
    let state = ov
        .edge_props
        .entry(edge.clone())
        .or_insert_with(|| EdgeState {
            props: None,
            delta: BTreeMap::new(),
        });
    let delta = std::mem::take(&mut state.delta);
    state.props.get_or_insert_with(|| {
        let mut props = src.row_edge_props(&edge.0, &edge.1, &edge.2);
        for (field, value) in delta {
            match value {
                Some(v) => props.insert(field, v),
                None => props.remove(&field),
            };
        }
        props
    })
}

/// Whether the edge exists at this point of the statement.
pub(super) fn uw_edge_exists<S: RowSource + ?Sized>(
    src: &S,
    ov: &Overlay,
    etype: &str,
    from: &str,
    to: &str,
) -> bool {
    if ov
        .edges
        .contains(&(etype.to_string(), from.to_string(), to.to_string()))
    {
        return true;
    }
    let fresh = |key: &str| ov.nodes.get(key).is_some_and(|n| n.created_here);
    if fresh(from) || fresh(to) {
        return false;
    }
    uw_exists(src, ov, from) && uw_exists(src, ov, to) && src.row_edge(etype, from, to)
}

/// Whether any edge touches `key` at this point of the statement.
pub(super) fn uw_has_edges<S: RowSource + ?Sized>(src: &S, ov: &Overlay, key: &str) -> bool {
    if ov.has_pending_edge(key) {
        return true;
    }
    if ov.nodes.get(key).is_some_and(|n| n.created_here) {
        return false;
    }
    src.row_has_edges(key)
}

/// The relationships a `DETACH DELETE` of `key` removes at this point of the
/// statement: the ones it created there, and the stored ones whose other end
/// an earlier row has not already deleted (that row removed and counted
/// them). A self-loop is one.
pub(super) fn uw_edges_removed<S: RowSource + ?Sized>(src: &S, ov: &Overlay, key: &str) -> i64 {
    let mut removed = ov.pending_edges_at(key);
    if !ov.nodes.get(key).is_some_and(|n| n.created_here) {
        removed += src
            .row_incident(key)
            .iter()
            .filter(|other| {
                other.as_str() == key
                    || !ov
                        .nodes
                        .get(other.as_str())
                        .is_some_and(|n| !n.exists || n.created_here)
            })
            .count();
    }
    removed as i64
}

/// Record in the overlay what `SET n = {…}` / `SET n += {…}` does to `key`.
pub(super) fn uw_apply_map<S: RowSource + ?Sized>(
    src: &S,
    ov: &mut Overlay,
    key: &str,
    merge: bool,
    entries: &MapEntries,
) {
    let existing = uw_props(src, ov, key).clone();
    let writes = plan_map_write(key, &existing, merge, entries, &|field| {
        src.row_view_owns(field)
    });
    for write in writes {
        match write {
            MapWrite::Set { field, value } => ov.note_set(key, &field, &value),
            MapWrite::Remove { field } => ov.note_remove(key, &field),
        }
    }
}

/// A property of a node as a read of it would see it at this point of the
/// statement: the stored or written value, else the identity the engine
/// exposes as a property (`key`, `id`, `label`).
fn uw_read_prop<S: RowSource + ?Sized>(
    src: &S,
    ov: &mut Overlay,
    key: &str,
    field: &str,
) -> Option<Value> {
    if !uw_exists(src, ov, key) {
        return None;
    }
    if let Some(v) = uw_props(src, ov, key).get(field) {
        return Some(v.clone());
    }
    match field {
        "key" | "id" => Some(Value::Str(key.to_string())),
        "label" => src.row_label(key).map(Value::Str),
        _ => None,
    }
}

/// Evaluate `operand` for one row at this point of the statement.
///
/// `node_key` answers for a name: `Some(Some(key))` a node, `Some(None)` a
/// node variable that is null in this row (an `OPTIONAL MATCH` that found
/// nothing), `None` not a node. `values` are the row's other variables.
///
/// `rel_key` answers for a relationship name the same way (QL-RP): a property
/// of it reads the statement's own writes before the store.
pub(super) fn uw_eval<S: RowSource + ?Sized>(
    src: &S,
    ov: &mut Overlay,
    operand: &Operand,
    node_key: &dyn Fn(&str) -> Option<Option<String>>,
    rel_key: &dyn Fn(&str) -> Option<Option<EdgeKey>>,
    values: &[(&str, Value)],
    params: &BTreeMap<String, Value>,
) -> Result<Option<Value>> {
    let mut stored: Vec<(String, u32)> = Vec::new();
    let mut rels: Vec<(String, (u32, u32, u32))> = Vec::new();
    let rewritten = map_leaves(operand, &mut |leaf| match leaf {
        Operand::Prop { var, field } if node_key(var).is_none() => {
            let edge = rel_key(var)?;
            Some(Operand::Lit(
                edge.and_then(|e| uw_edge_props(src, ov, &e).get(field).cloned())
                    .unwrap_or(Value::Null),
            ))
        }
        Operand::Var(var) if node_key(var).is_none() => {
            let edge = rel_key(var)?;
            match edge.and_then(|e| src.row_edge_ids(&e.0, &e.1, &e.2)) {
                Some(ids) => {
                    rels.push((var.clone(), ids));
                    Some(Operand::Var(var.clone()))
                }
                None => Some(Operand::Lit(Value::Null)),
            }
        }
        Operand::Prop { var, field } => {
            let key = node_key(var)?;
            Some(match key {
                Some(key) => match uw_read_prop(src, ov, &key, field) {
                    Some(v) => Operand::Lit(v),
                    None => Operand::Lit(Value::Null),
                },
                None => Operand::Lit(Value::Null),
            })
        }
        Operand::Var(var) => {
            let key = node_key(var)?;
            Some(match key {
                Some(key) => match src.row_id(&key).filter(|_| uw_exists(src, ov, &key)) {
                    Some(id) if !ov.nodes.get(&key).is_some_and(|n| n.created_here) => {
                        stored.push((var.clone(), id));
                        Operand::Var(var.clone())
                    }
                    _ => Operand::Lit(Value::Str(key)),
                },
                None => Operand::Lit(Value::Null),
            })
        }
        _ => None,
    });
    let mut bindings: Vec<(&str, Binding)> = values
        .iter()
        .map(|(name, v)| (*name, Binding::Value(v.clone())))
        .collect();
    for (name, id) in &stored {
        bindings.push((name.as_str(), Binding::Node(*id)));
    }
    for (name, (etype, src, dst)) in &rels {
        bindings.push((
            name.as_str(),
            Binding::Rel {
                etype: *etype,
                src: *src,
                dst: *dst,
            },
        ));
    }
    src.row_view(|view| eval_operand(view, &rewritten, &Params(params), &bindings))
        .map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })
}

/// Whether `operand` reads a node variable in `nodes`: a property of it, or the
/// bare name.
pub(super) fn reads_node(operand: &Operand, nodes: &[String]) -> bool {
    let mut found = false;
    map_leaves(operand, &mut |leaf| {
        let name = match leaf {
            Operand::Prop { var, .. } | Operand::Var(var) => var,
            _ => return None,
        };
        if nodes.iter().any(|n| n == name) {
            found = true;
        }
        None
    });
    found
}

/// Whether `operand` reads a property of a relationship variable in `rels`.
pub(super) fn reads_rel_prop(operand: &Operand, rels: &[String]) -> bool {
    let mut found = false;
    map_leaves(operand, &mut |leaf| {
        if let Operand::Prop { var, .. } = leaf {
            if rels.iter().any(|r| r == var) {
                found = true;
            }
        }
        None
    });
    found
}

/// A copy of `operand` in which every `Prop` and `Var` leaf for which `leaf`
/// returns `Some(replacement)` is replaced. The walk covers the conditions of a
/// `CASE` as well.
pub(super) fn map_leaves(
    operand: &Operand,
    leaf: &mut dyn FnMut(&Operand) -> Option<Operand>,
) -> Operand {
    match operand {
        Operand::Prop { .. } | Operand::Var(_) => leaf(operand).unwrap_or_else(|| operand.clone()),
        Operand::Lit(_) | Operand::Param(_) => operand.clone(),
        Operand::FuncCall { name, args } => Operand::FuncCall {
            name: name.clone(),
            args: args.iter().map(|a| map_leaves(a, leaf)).collect(),
        },
        Operand::BinArith { op, left, right } => Operand::BinArith {
            op: op.clone(),
            left: Box::new(map_leaves(left, leaf)),
            right: Box::new(map_leaves(right, leaf)),
        },
        Operand::Index { base, index } => Operand::Index {
            base: Box::new(map_leaves(base, leaf)),
            index: Box::new(map_leaves(index, leaf)),
        },
        Operand::ListLit(items) => {
            Operand::ListLit(items.iter().map(|i| map_leaves(i, leaf)).collect())
        }
        Operand::MapLit(entries) => Operand::MapLit(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), map_leaves(v, leaf)))
                .collect(),
        ),
        Operand::Case { branches, default } => Operand::Case {
            branches: branches
                .iter()
                .map(|(cond, value)| (map_expr(cond, leaf), map_leaves(value, leaf)))
                .collect(),
            default: default.as_ref().map(|d| Box::new(map_leaves(d, leaf))),
        },
        // The list (and `reduce`'s initial value) is read in the enclosing
        // scope; the body is read with the comprehension's own variables
        // bound, so a leaf naming one of them is left alone.
        Operand::ListComprehension {
            var,
            list,
            filter,
            map,
        } => {
            let list = Box::new(map_leaves(list, leaf));
            let locals = [var];
            let mut inner = shadowing(&locals, leaf);
            Operand::ListComprehension {
                var: var.clone(),
                list,
                filter: filter.as_ref().map(|f| Box::new(map_expr(f, &mut inner))),
                map: Box::new(map_leaves(map, &mut inner)),
            }
        }
        Operand::ListPredicate {
            kind,
            var,
            list,
            pred,
        } => {
            let list = Box::new(map_leaves(list, leaf));
            let locals = [var];
            let mut inner = shadowing(&locals, leaf);
            Operand::ListPredicate {
                kind: *kind,
                var: var.clone(),
                list,
                pred: Box::new(map_expr(pred, &mut inner)),
            }
        }
        Operand::Reduce {
            acc_var,
            init,
            iter_var,
            list,
            body,
        } => {
            let init = Box::new(map_leaves(init, leaf));
            let list = Box::new(map_leaves(list, leaf));
            let locals = [acc_var, iter_var];
            let mut inner = shadowing(&locals, leaf);
            Operand::Reduce {
                acc_var: acc_var.clone(),
                init,
                iter_var: iter_var.clone(),
                list,
                body: Box::new(map_leaves(body, &mut inner)),
            }
        }
        Operand::Pred(expr) => Operand::Pred(Box::new(map_expr(expr, leaf))),
    }
}

/// `leaf`, except that a `Prop` or `Var` leaf naming one of `locals` is kept:
/// inside a list comprehension or `reduce` that name is the local variable.
fn shadowing<'a>(
    locals: &'a [&'a String],
    leaf: &'a mut dyn FnMut(&Operand) -> Option<Operand>,
) -> impl FnMut(&Operand) -> Option<Operand> + 'a {
    move |operand| {
        let name = match operand {
            Operand::Prop { var, .. } | Operand::Var(var) => var,
            _ => return leaf(operand),
        };
        if locals.contains(&name) {
            None
        } else {
            leaf(operand)
        }
    }
}

fn map_expr(expr: &Expr, leaf: &mut dyn FnMut(&Operand) -> Option<Operand>) -> Expr {
    match expr {
        Expr::And(a, b) => Expr::And(Box::new(map_expr(a, leaf)), Box::new(map_expr(b, leaf))),
        Expr::Or(a, b) => Expr::Or(Box::new(map_expr(a, leaf)), Box::new(map_expr(b, leaf))),
        Expr::Xor(a, b) => Expr::Xor(Box::new(map_expr(a, leaf)), Box::new(map_expr(b, leaf))),
        Expr::Not(a) => Expr::Not(Box::new(map_expr(a, leaf))),
        Expr::Cmp { lhs, op, rhs } => Expr::Cmp {
            lhs: map_leaves(lhs, leaf),
            op: op.clone(),
            rhs: map_leaves(rhs, leaf),
        },
        Expr::Truthy(o) => Expr::Truthy(map_leaves(o, leaf)),
        Expr::IsNull(o) => Expr::IsNull(map_leaves(o, leaf)),
        Expr::IsNotNull(o) => Expr::IsNotNull(map_leaves(o, leaf)),
        Expr::In { expr, list } => Expr::In {
            expr: map_leaves(expr, leaf),
            list: map_leaves(list, leaf),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pending-edge check of a plain `DELETE` is one lookup, not a scan of
    /// every edge the statement made: 100,000 edges and 100,000 checks finish
    /// at once (a scan would be 10^10 comparisons).
    #[test]
    fn the_pending_edge_check_is_a_lookup() {
        let mut ov = Overlay::default();
        for i in 0..100_000 {
            ov.edge_created("T", &format!("a{i}"), &format!("b{i}"));
        }
        let started = std::time::Instant::now();
        let touched = (0..100_000)
            .filter(|i| ov.has_pending_edge(&format!("a{i}")))
            .count();
        assert_eq!(touched, 100_000);
        assert!(!ov.has_pending_edge("nobody"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn deleting_a_node_drops_the_pending_edges_it_held() {
        let mut ov = Overlay::default();
        ov.edge_created("T", "a", "b");
        assert!(ov.has_pending_edge("b"));
        ov.node_deleted("a");
        assert!(!ov.has_pending_edge("a"));
        assert!(!ov.has_pending_edge("b"), "the edge is gone with its end");
    }
}
