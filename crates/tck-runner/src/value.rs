//! The values of a TCK result table, and how an engine result cell compares
//! with one.
//!
//! The TCK writes every expected value as text (`1`, `'a'`, `[1, 2]`,
//! `{a: 1}`, `(:A {name: 'x'})`, `[:T]`, `<(:A)-[:T]->(:B)>`). [`parse`] reads
//! that text into a [`TckValue`]; [`from_cell`] turns one engine cell — its
//! value and, for a graph-element column, the element behind it — into the
//! same shape, so the two compare structurally.
//!
//! Temporal values have no literal form in the TCK: an expected date is the
//! string of its canonical text (`'1984-10-11'`). An engine temporal value is
//! kept as [`TckValue::Temporal`] and matches an expected string that parses
//! to the same value, or that equals its displayed text.

use mushroomdb::{tagged_text, value_from_tagged_text, Entity, NodeEntity, RelationshipEntity};
use mushroomdb::{v11_display_text, Value};
use std::collections::BTreeMap;
use std::fmt;

/// One value of a TCK table, or of an engine result converted for comparison.
#[derive(Debug, Clone)]
pub enum TckValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<TckValue>),
    Map(BTreeMap<String, TckValue>),
    Node(TckNode),
    Rel(TckRel),
    /// The first node, then each step: the relationship, whether it points
    /// forward (left to right), and the node it reaches.
    Path(TckNode, Vec<(TckRel, bool, TckNode)>),
    /// An engine temporal value (never parsed from a table).
    Temporal(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TckNode {
    pub labels: Vec<String>,
    pub props: BTreeMap<String, TckValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TckRel {
    pub rel_type: String,
    pub props: BTreeMap<String, TckValue>,
}

impl PartialEq for TckValue {
    fn eq(&self, other: &Self) -> bool {
        values_equal(self, other, false)
    }
}

/// Whether two values are the same TCK value. `unordered_lists` compares
/// lists as multisets ("ignoring element order for lists").
pub fn values_equal(a: &TckValue, b: &TckValue, unordered_lists: bool) -> bool {
    use TckValue::*;
    match (a, b) {
        (Null, Null) => true,
        (Bool(x), Bool(y)) => x == y,
        (Int(x), Int(y)) => x == y,
        (Float(x), Float(y)) => (x.is_nan() && y.is_nan()) || x == y,
        (Str(x), Str(y)) => x == y,
        (Temporal(t), Str(s)) | (Str(s), Temporal(t)) => temporal_matches(t, s),
        (Temporal(x), Temporal(y)) => x == y,
        (List(x), List(y)) => {
            if x.len() != y.len() {
                return false;
            }
            if unordered_lists {
                multiset_equal(x, y, |p, q| values_equal(p, q, true))
            } else {
                x.iter()
                    .zip(y)
                    .all(|(p, q)| values_equal(p, q, unordered_lists))
            }
        }
        (Map(x), Map(y)) => maps_equal(x, y, unordered_lists),
        (Node(x), Node(y)) => nodes_equal(x, y),
        (Rel(x), Rel(y)) => rels_equal(x, y),
        (Path(s1, steps1), Path(s2, steps2)) => {
            nodes_equal(s1, s2)
                && steps1.len() == steps2.len()
                && steps1
                    .iter()
                    .zip(steps2)
                    .all(|((r1, f1, n1), (r2, f2, n2))| {
                        f1 == f2 && rels_equal(r1, r2) && nodes_equal(n1, n2)
                    })
        }
        _ => false,
    }
}

fn maps_equal(
    x: &BTreeMap<String, TckValue>,
    y: &BTreeMap<String, TckValue>,
    unordered_lists: bool,
) -> bool {
    x.len() == y.len()
        && x.iter().all(|(k, v)| {
            y.get(k)
                .is_some_and(|w| values_equal(v, w, unordered_lists))
        })
}

fn nodes_equal(x: &TckNode, y: &TckNode) -> bool {
    let mut lx = x.labels.clone();
    let mut ly = y.labels.clone();
    lx.sort();
    ly.sort();
    lx == ly && maps_equal(&x.props, &y.props, false)
}

fn rels_equal(x: &TckRel, y: &TckRel) -> bool {
    x.rel_type == y.rel_type && maps_equal(&x.props, &y.props, false)
}

/// Whether `xs` and `ys` hold the same elements, counted, under `eq` (an
/// equivalence, so a greedy match is exact).
pub fn multiset_equal<T>(xs: &[T], ys: &[T], eq: impl Fn(&T, &T) -> bool) -> bool {
    if xs.len() != ys.len() {
        return false;
    }
    let mut used = vec![false; ys.len()];
    'outer: for x in xs {
        for (i, y) in ys.iter().enumerate() {
            if !used[i] && eq(x, y) {
                used[i] = true;
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// An engine temporal value against the TCK's string for it: the same value
/// once parsed, or the same text.
fn temporal_matches(value: &Value, expected: &str) -> bool {
    if v11_display_text(value).as_deref() == Some(expected) {
        return true;
    }
    match tagged_text(value) {
        Ok(Some((tag, _))) => value_from_tagged_text(tag, expected).is_ok_and(|v| &v == value),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Engine cell -> TckValue

/// One engine result cell as a TCK value: `entity` (when the column holds
/// graph elements and details were attached) wins over the rendered value.
pub fn from_cell(entity: Option<&Entity>, value: Option<&Value>) -> TckValue {
    match entity {
        Some(e) => from_entity(e),
        None => value.map_or(TckValue::Null, from_value),
    }
}

pub fn from_value(v: &Value) -> TckValue {
    match v {
        Value::Null => TckValue::Null,
        Value::Bool(b) => TckValue::Bool(*b),
        Value::Int(i) => TckValue::Int(*i),
        Value::Float(f) => TckValue::Float(*f),
        Value::Str(s) => TckValue::Str(s.clone()),
        Value::List(items) => TckValue::List(items.iter().map(from_value).collect()),
        Value::Map(m) => TckValue::Map(m.iter().map(|(k, v)| (k.clone(), from_value(v))).collect()),
        Value::Binary(_) => TckValue::Str(v11_display_text(v).unwrap_or_default()),
        other => TckValue::Temporal(other.clone()),
    }
}

fn from_entity(e: &Entity) -> TckValue {
    match e {
        Entity::Node(n) => TckValue::Node(node_of(n)),
        Entity::Relationship(r) => TckValue::Rel(rel_of(r)),
        Entity::Path(p) => {
            let Some(first) = p.nodes.first() else {
                return TckValue::Null;
            };
            let mut steps = Vec::with_capacity(p.relationships.len());
            let mut prev = first;
            for (rel, next) in p.relationships.iter().zip(p.nodes.iter().skip(1)) {
                let forward = rel.start_id == prev.id;
                steps.push((rel_of(rel), forward, node_of(next)));
                prev = next;
            }
            TckValue::Path(node_of(first), steps)
        }
        Entity::List(items) => TckValue::List(
            items
                .iter()
                .map(|i| i.as_ref().map_or(TckValue::Null, from_entity))
                .collect(),
        ),
    }
}

/// A node's labels as the TCK sees them: a node with no label (an empty
/// label string) has none.
pub fn node_of(n: &NodeEntity) -> TckNode {
    TckNode {
        labels: if n.label.is_empty() {
            Vec::new()
        } else {
            vec![n.label.clone()]
        },
        props: n
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), from_value(v)))
            .collect(),
    }
}

pub fn rel_of(r: &RelationshipEntity) -> TckRel {
    TckRel {
        rel_type: r.rel_type.clone(),
        props: r
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), from_value(v)))
            .collect(),
    }
}

/// A TCK value as an engine parameter. Graph elements have no parameter form.
pub fn to_param(v: &TckValue) -> Option<Value> {
    Some(match v {
        TckValue::Null => Value::Null,
        TckValue::Bool(b) => Value::Bool(*b),
        TckValue::Int(i) => Value::Int(*i),
        TckValue::Float(f) => Value::Float(*f),
        TckValue::Str(s) => Value::Str(s.clone()),
        TckValue::List(items) => Value::List(items.iter().map(to_param).collect::<Option<_>>()?),
        TckValue::Map(m) => Value::Map(
            m.iter()
                .map(|(k, v)| Some((k.clone(), to_param(v)?)))
                .collect::<Option<_>>()?,
        ),
        TckValue::Temporal(t) => t.clone(),
        TckValue::Node(_) | TckValue::Rel(_) | TckValue::Path(..) => return None,
    })
}

// ---------------------------------------------------------------------------
// Display (the TCK's own notation, used in failure messages)

impl fmt::Display for TckValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TckValue::Null => write!(f, "null"),
            TckValue::Bool(b) => write!(f, "{b}"),
            TckValue::Int(i) => write!(f, "{i}"),
            TckValue::Float(x) => {
                if x.is_nan() {
                    write!(f, "NaN")
                } else if x.is_infinite() {
                    write!(f, "{}", if *x > 0.0 { "Inf" } else { "-Inf" })
                } else if x.fract() == 0.0 && x.abs() < 1e16 {
                    write!(f, "{x:.1}")
                } else {
                    write!(f, "{x}")
                }
            }
            TckValue::Str(s) => write!(f, "'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
            TckValue::List(items) => {
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            TckValue::Map(m) => write_map(f, m),
            TckValue::Node(n) => write_node(f, n),
            TckValue::Rel(r) => write_rel(f, r),
            TckValue::Path(start, steps) => {
                write!(f, "<")?;
                write_node(f, start)?;
                for (rel, forward, node) in steps {
                    if *forward {
                        write!(f, "-")?;
                        write_rel(f, rel)?;
                        write!(f, "->")?;
                    } else {
                        write!(f, "<-")?;
                        write_rel(f, rel)?;
                        write!(f, "-")?;
                    }
                    write_node(f, node)?;
                }
                write!(f, ">")
            }
            TckValue::Temporal(t) => write!(
                f,
                "'{}'",
                v11_display_text(t).unwrap_or_else(|| format!("{t:?}"))
            ),
        }
    }
}

fn write_map(f: &mut fmt::Formatter<'_>, m: &BTreeMap<String, TckValue>) -> fmt::Result {
    write!(f, "{{")?;
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{k}: {v}")?;
    }
    write!(f, "}}")
}

fn write_node(f: &mut fmt::Formatter<'_>, n: &TckNode) -> fmt::Result {
    write!(f, "(")?;
    for l in &n.labels {
        write!(f, ":{l}")?;
    }
    if !n.props.is_empty() {
        if !n.labels.is_empty() {
            write!(f, " ")?;
        }
        write_map(f, &n.props)?;
    }
    write!(f, ")")
}

fn write_rel(f: &mut fmt::Formatter<'_>, r: &TckRel) -> fmt::Result {
    write!(f, "[:{}", r.rel_type)?;
    if !r.props.is_empty() {
        write!(f, " ")?;
        write_map(f, &r.props)?;
    }
    write!(f, "]")
}

// ---------------------------------------------------------------------------
// Parser for the TCK's value notation

/// Parse one TCK table cell (or parameter value).
pub fn parse(text: &str) -> Result<TckValue, String> {
    let mut p = Parser {
        s: text.as_bytes(),
        src: text,
        i: 0,
    };
    let v = p.value()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("trailing text in TCK value {text:?} at {}", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!(
                "expected '{}' at {} in {:?}",
                c as char, self.i, self.src
            ))
        }
    }

    fn starts_with(&self, word: &str) -> bool {
        self.src[self.i..].starts_with(word)
    }

    fn value(&mut self) -> Result<TckValue, String> {
        self.ws();
        match self.peek() {
            None => Err(format!("empty TCK value {:?}", self.src)),
            Some(b'\'') | Some(b'"') => self.string().map(TckValue::Str),
            Some(b'[') => {
                // `[:T …]` is a relationship; anything else a list.
                let save = self.i;
                self.i += 1;
                self.ws();
                let is_rel = self.peek() == Some(b':');
                self.i = save;
                if is_rel {
                    self.rel().map(TckValue::Rel)
                } else {
                    self.list()
                }
            }
            Some(b'{') => self.map().map(TckValue::Map),
            Some(b'(') => self.node().map(TckValue::Node),
            Some(b'<') => self.path(),
            _ => self.scalar(),
        }
    }

    fn scalar(&mut self) -> Result<TckValue, String> {
        for (word, v) in [
            ("null", TckValue::Null),
            ("true", TckValue::Bool(true)),
            ("false", TckValue::Bool(false)),
            ("NaN", TckValue::Float(f64::NAN)),
            ("-Infinity", TckValue::Float(f64::NEG_INFINITY)),
            ("Infinity", TckValue::Float(f64::INFINITY)),
            ("-Inf", TckValue::Float(f64::NEG_INFINITY)),
            ("Inf", TckValue::Float(f64::INFINITY)),
        ] {
            if self.starts_with(word) {
                self.i += word.len();
                return Ok(v);
            }
        }
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.' | b'_') {
                self.i += 1;
            } else {
                break;
            }
        }
        let tok = &self.src[start..self.i];
        if tok.is_empty() {
            return Err(format!("unexpected character at {start} in {:?}", self.src));
        }
        if let Ok(i) = tok.parse::<i64>() {
            return Ok(TckValue::Int(i));
        }
        if let Some(hex) = tok.strip_prefix("0x") {
            if let Ok(i) = i64::from_str_radix(hex, 16) {
                return Ok(TckValue::Int(i));
            }
        }
        tok.parse::<f64>()
            .map(TckValue::Float)
            .map_err(|_| format!("not a TCK value: {tok:?} in {:?}", self.src))
    }

    fn string(&mut self) -> Result<String, String> {
        let quote = self.s[self.i];
        self.i += 1;
        let mut out = String::new();
        let rest = &self.src[self.i..];
        let mut chars = rest.char_indices();
        while let Some((off, c)) = chars.next() {
            if c == quote as char {
                self.i += off + 1;
                return Ok(out);
            }
            if c == '\\' {
                match chars.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, 'r')) => out.push('\r'),
                    Some((_, 'b')) => out.push('\u{8}'),
                    Some((_, 'f')) => out.push('\u{c}'),
                    Some((_, 'u')) => {
                        let hex: String =
                            (0..4).filter_map(|_| chars.next().map(|x| x.1)).collect();
                        let code = u32::from_str_radix(&hex, 16)
                            .map_err(|_| format!("bad \\u escape in {:?}", self.src))?;
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    }
                    Some((_, other)) => out.push(other),
                    None => break,
                }
            } else {
                out.push(c);
            }
        }
        Err(format!("unterminated string in {:?}", self.src))
    }

    fn list(&mut self) -> Result<TckValue, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        if self.eat(b']') {
            return Ok(TckValue::List(items));
        }
        loop {
            items.push(self.value()?);
            if self.eat(b']') {
                return Ok(TckValue::List(items));
            }
            self.expect(b',')?;
        }
    }

    fn key(&mut self) -> Result<String, String> {
        self.ws();
        if self.peek() == Some(b'`') {
            self.i += 1;
            let start = self.i;
            while self.peek().is_some_and(|c| c != b'`') {
                self.i += 1;
            }
            let k = self.src[start..self.i].to_string();
            self.expect(b'`')?;
            return Ok(k);
        }
        if matches!(self.peek(), Some(b'\'') | Some(b'"')) {
            return self.string();
        }
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80)
        {
            self.i += 1;
        }
        if start == self.i {
            return Err(format!("expected a name at {start} in {:?}", self.src));
        }
        Ok(self.src[start..self.i].to_string())
    }

    fn map(&mut self) -> Result<BTreeMap<String, TckValue>, String> {
        self.expect(b'{')?;
        let mut m = BTreeMap::new();
        if self.eat(b'}') {
            return Ok(m);
        }
        loop {
            let k = self.key()?;
            self.expect(b':')?;
            let v = self.value()?;
            m.insert(k, v);
            if self.eat(b'}') {
                return Ok(m);
            }
            self.expect(b',')?;
        }
    }

    fn node(&mut self) -> Result<TckNode, String> {
        self.expect(b'(')?;
        let mut labels = Vec::new();
        while self.eat(b':') {
            labels.push(self.key()?);
        }
        self.ws();
        let props = if self.peek() == Some(b'{') {
            self.map()?
        } else {
            BTreeMap::new()
        };
        self.expect(b')')?;
        Ok(TckNode { labels, props })
    }

    fn rel(&mut self) -> Result<TckRel, String> {
        self.expect(b'[')?;
        self.expect(b':')?;
        let rel_type = self.key()?;
        self.ws();
        let props = if self.peek() == Some(b'{') {
            self.map()?
        } else {
            BTreeMap::new()
        };
        self.expect(b']')?;
        Ok(TckRel { rel_type, props })
    }

    fn path(&mut self) -> Result<TckValue, String> {
        self.expect(b'<')?;
        let start = self.node()?;
        let mut steps = Vec::new();
        loop {
            self.ws();
            if self.eat(b'>') {
                return Ok(TckValue::Path(start, steps));
            }
            if self.eat(b'<') {
                self.expect(b'-')?;
                let rel = self.rel()?;
                self.expect(b'-')?;
                let node = self.node()?;
                steps.push((rel, false, node));
            } else {
                self.expect(b'-')?;
                let rel = self.rel()?;
                self.expect(b'-')?;
                self.expect(b'>')?;
                let node = self.node()?;
                steps.push((rel, true, node));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_notation() {
        assert_eq!(parse("1").unwrap(), TckValue::Int(1));
        assert_eq!(parse("-1.5e3").unwrap(), TckValue::Float(-1500.0));
        assert_eq!(parse("'it\\'s'").unwrap(), TckValue::Str("it's".into()));
        assert_eq!(
            parse("[1, 'a', null]").unwrap(),
            TckValue::List(vec![
                TckValue::Int(1),
                TckValue::Str("a".into()),
                TckValue::Null
            ])
        );
        let node = parse("(:A:B {name: 'x'})").unwrap();
        assert_eq!(node.to_string(), "(:A:B {name: 'x'})");
        let path = parse("<(:A)-[:T {k: 1}]->(:B)<-[:S]-()>").unwrap();
        assert_eq!(path.to_string(), "<(:A)-[:T {k: 1}]->(:B)<-[:S]-()>");
        assert_eq!(parse("[]").unwrap(), TckValue::List(vec![]));
        assert_eq!(parse("{}").unwrap(), TckValue::Map(BTreeMap::new()));
        assert!(matches!(parse("NaN").unwrap(), TckValue::Float(f) if f.is_nan()));
    }

    #[test]
    fn int_and_float_differ_and_lists_can_ignore_order() {
        assert_ne!(TckValue::Int(1), TckValue::Float(1.0));
        let a = parse("[1, 2]").unwrap();
        let b = parse("[2, 1]").unwrap();
        assert!(!values_equal(&a, &b, false));
        assert!(values_equal(&a, &b, true));
    }
}
