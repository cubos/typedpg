//! The type facts [`crate::PgCatalog::query_view`] records for the lint
//! rules: how each expression's value is converted to the type its context
//! takes (an operator's or function's declared argument type, a `CASE` /
//! `COALESCE` / `UNION` common type, an assigned column's type, an explicit
//! cast), and what types each operator call combines.
//!
//! A fact is keyed by the node's own location in [`crate::QueryView::sql`]
//! (and, for conversions, the node's kind). A site the analyzer infers more
//! than once records the same fact each time; two different facts for one
//! key (a site inferred two ways, or a node of an internal analysis sharing
//! the offset) leave no fact. Internal analyses — a stored default, an
//! inlined SQL function body, anything run under [`crate::ddl::depend`]'s
//! `collect` — record nothing: their offsets are in another text.

use std::cell::RefCell;
use std::collections::HashMap;

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast;

use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, oid};

/// One conversion of an expression's value to another type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueConversion {
    /// The expression's own type (a domain is its base type).
    pub from: PgTypeOid,
    /// The type its context converts it to (a domain is its base type).
    pub to: PgTypeOid,
    /// Whether the conversion is a written cast (`x::t`, `CAST(x AS t)`)
    /// rather than one PostgreSQL applies implicitly.
    pub explicit: bool,
}

/// The types an operator call combines, as its operands are before any
/// conversion to the operator's declared argument types (a domain is its
/// base type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorTypes {
    /// The left operand's type; `None` for a prefix operator. `unknown` for
    /// an untyped literal or parameter typed by the operator.
    pub left: Option<PgTypeOid>,
    /// The right operand's type, `unknown` as for [`Self::left`].
    pub right: PgTypeOid,
    /// The operator's result type.
    pub result: PgTypeOid,
}

/// A fact one key recorded, or `Conflict` when two differed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fact<T> {
    One(T),
    Conflict,
}

fn merge<K: std::hash::Hash + Eq, T: PartialEq>(map: &mut HashMap<K, Fact<T>>, key: K, value: T) {
    match map.entry(key) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if !matches!(entry.get(), Fact::One(v) if *v == value) {
                entry.insert(Fact::Conflict);
            }
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Fact::One(value));
        }
    }
}

/// A node's location and kind (its `NodeRef` variant, hashed).
type NodeKey = (i32, u64);

/// The facts collected while analyzing one query.
#[derive(Debug, Clone, Default)]
pub(crate) struct TypeFacts {
    /// Per coerced node: the conversion, or `None` when the node met a
    /// coercion site already of the target type.
    conversions: HashMap<NodeKey, Fact<Option<ValueConversion>>>,
    /// Per operator call (`A_Expr`) location.
    operators: HashMap<i32, Fact<OperatorTypes>>,
}

impl TypeFacts {
    pub(crate) fn conversion(&self, node: NodeRef<'_>) -> Option<ValueConversion> {
        match self.conversions.get(&key(node)?)? {
            Fact::One(conversion) => *conversion,
            Fact::Conflict => None,
        }
    }

    pub(crate) fn operator(&self, location: i32) -> Option<OperatorTypes> {
        match self.operators.get(&location)? {
            Fact::One(types) => Some(*types),
            Fact::Conflict => None,
        }
    }
}

fn key(node: NodeRef<'_>) -> Option<NodeKey> {
    use std::hash::{Hash, Hasher};
    let location = node.location()?;
    let mut kind = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(&node).hash(&mut kind);
    Some((location, kind.finish()))
}

thread_local! {
    /// Set while [`crate::PgCatalog::query_view`] analyzes a query, `None`
    /// otherwise (and during an internal analysis).
    static FACTS: RefCell<Option<TypeFacts>> = const { RefCell::new(None) };
}

/// Run `f`, collecting the type facts it records.
pub(crate) fn collecting<R>(f: impl FnOnce() -> R) -> (R, TypeFacts) {
    let before = FACTS.with(|facts| facts.replace(Some(TypeFacts::default())));
    let out = f();
    let collected = FACTS
        .with(|facts| facts.replace(before))
        .unwrap_or_default();
    (out, collected)
}

/// Run `f`, an internal analysis whose nodes are not the query's, recording
/// none of its facts.
pub(crate) fn suspended<R>(f: impl FnOnce() -> R) -> R {
    let before = FACTS.with(|facts| facts.replace(None));
    let out = f();
    FACTS.with(|facts| facts.replace(before));
    out
}

fn collecting_now() -> bool {
    FACTS.with(|facts| facts.borrow().is_some())
}

/// Record that the value of `node`, of type `from`, is converted to `to`
/// (an untyped literal or parameter, or a value already of type `to`, is
/// recorded as converted to nothing). A `NamedArgExpr` stands for its
/// argument.
pub(crate) fn note_conversion(
    node: Option<&ast::Node>,
    from: PgTypeOid,
    to: PgTypeOid,
    explicit: bool,
    snapshot: &PgCatalog,
) {
    if !collecting_now() {
        return;
    }
    let node = match node {
        Some(ast::Node::NamedArgExpr(named)) => named.arg.as_ref(),
        other => other,
    };
    let Some(key) = node.and_then(|node| key(node.to_ref())) else {
        return;
    };
    let from = snapshot.unwrap_domain(from);
    let to = snapshot.unwrap_domain(to);
    let conversion = (from != oid::UNKNOWN && to != oid::UNKNOWN && from != to)
        .then_some(ValueConversion { from, to, explicit });
    FACTS.with(|facts| {
        if let Some(facts) = facts.borrow_mut().as_mut() {
            merge(&mut facts.conversions, key, conversion);
        }
    });
}

/// Record the operand and result types of the operator call at `location`
/// (a domain stands for its base type).
pub(crate) fn note_operator(location: i32, types: OperatorTypes, snapshot: &PgCatalog) {
    if location < 0 || !collecting_now() {
        return;
    }
    let types = OperatorTypes {
        left: types.left.map(|t| snapshot.unwrap_domain(t)),
        right: snapshot.unwrap_domain(types.right),
        result: snapshot.unwrap_domain(types.result),
    };
    FACTS.with(|facts| {
        if let Some(facts) = facts.borrow_mut().as_mut() {
            merge(&mut facts.operators, location, types);
        }
    });
}
