//! What a qual says about a column it isn't strict in: evaluate it with
//! the column replaced by NULL — every other column unknown — and fold
//! what can be folded. `coalesce(b, 0) > 0` is `0 > 0`, FALSE, for a NULL
//! `b`: so a row it passes has `b` non-NULL, although `coalesce` is not
//! strict. Likewise a WHEN that folds to TRUE for a NULL `b`
//! (`coalesce(b, 0) = 0`, `num_nulls(b) > 0`) leaves `b` non-NULL in the
//! branches after it. PG itself proves nothing of the sort.
//!
//! The evaluation is an abstraction of the real one: [`Val::Unknown`] is
//! any value (NULL included), [`Val::NonNull`] any non-NULL one, and a
//! node only folds to a definite value when it has that value whatever
//! the unknowns are (an evaluation that raises an error instead yields no
//! row, which is fine for every caller).

use std::collections::BTreeSet;

use typedpg_pg_query::protobuf::{self, a_const::Val as ConstVal, node};

use super::{Col, StrictLog, StrictNode, plain_column};
use crate::pg_catalog::PgCatalog;
use crate::scope::Scope;

/// An abstract value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
    /// Some non-NULL value.
    NonNull,
    /// Any value, NULL included.
    Unknown,
}

impl Val {
    fn known_non_null(&self) -> bool {
        matches!(
            self,
            Val::Bool(_) | Val::Int(_) | Val::Text(_) | Val::NonNull
        )
    }
}

/// Whether a COALESCE / GREATEST / LEAST of type `t` (modifier `typmod`)
/// keeps the constants [`Subst`] folds as written: an integer, numeric,
/// text or boolean result with no modifier. A float one rounds a large
/// integer (`coalesce(f, 16777217)` over a `real` is 16777216).
pub(crate) fn folds_exactly(
    t: crate::oid::PgTypeOid,
    typmod: Option<i32>,
    snapshot: &PgCatalog,
) -> bool {
    use crate::pg_catalog::oid;
    typmod.is_none_or(|m| m < 0)
        && [
            oid::INT2,
            oid::INT4,
            oid::INT8,
            oid::NUMERIC,
            oid::TEXT,
            oid::VARCHAR,
            oid::BOOL,
        ]
        .contains(&snapshot.unwrap_domain(t))
        && snapshot.effective_typmod(t, None).is_none_or(|m| m < 0)
}

/// Whether a COALESCE / GREATEST / LEAST of type `t` is a float one (see
/// [`StrictNode::FloatFold`]).
pub(crate) fn folds_as_float(t: crate::oid::PgTypeOid, snapshot: &PgCatalog) -> bool {
    use crate::pg_catalog::oid;
    [oid::FLOAT4, oid::FLOAT8].contains(&snapshot.unwrap_domain(t))
}

/// The largest integer every float type (`real`'s 24-bit mantissa) holds
/// exactly, with every smaller one.
const FLOAT_EXACT: i64 = 1 << 24;

/// What a qual evaluates to, with column `null` replaced by NULL.
pub(crate) struct Subst<'a> {
    pub null: &'a Col,
    pub scope: &'a Scope,
    pub log: &'a StrictLog,
    pub snapshot: &'a PgCatalog,
}

/// The largest qual (in nodes) and the most columns substituted.
const MAX_NODES: usize = 256;
const MAX_COLUMNS: usize = 16;

/// The plain columns of this level `n` references (not those inside a
/// sublink), if it is small enough to evaluate once per column.
pub(crate) fn candidate_columns(n: &protobuf::Node, scope: &Scope) -> Vec<Col> {
    let mut out = BTreeSet::new();
    let mut count = 0usize;
    if !collect(n, scope, &mut out, &mut count) || out.len() > MAX_COLUMNS {
        return Vec::new();
    }
    out.into_iter().collect()
}

fn collect(n: &protobuf::Node, scope: &Scope, out: &mut BTreeSet<Col>, count: &mut usize) -> bool {
    *count += 1;
    if *count > MAX_NODES {
        return false;
    }
    let mut each = |ns: &[protobuf::Node]| ns.iter().all(|c| collect(c, scope, out, count));
    match n.node.as_ref() {
        Some(node::Node::ColumnRef(_)) => {
            if let Some((c, _)) = plain_column(n, scope) {
                out.insert(c);
            }
            true
        }
        Some(node::Node::AExpr(e)) => {
            let kids: Vec<protobuf::Node> = [&e.lexpr, &e.rexpr]
                .into_iter()
                .flatten()
                .map(|b| (**b).clone())
                .collect();
            each(&kids)
        }
        Some(node::Node::BoolExpr(b)) => each(&b.args),
        Some(node::Node::FuncCall(f)) => each(&f.args),
        Some(node::Node::List(l)) => each(&l.items),
        Some(node::Node::CoalesceExpr(c)) => each(&c.args),
        Some(node::Node::MinMaxExpr(m)) => each(&m.args),
        Some(node::Node::RowExpr(r)) => each(&r.args),
        Some(node::Node::TypeCast(c)) => c
            .arg
            .as_deref()
            .is_none_or(|a| collect(a, scope, out, count)),
        Some(node::Node::CollateClause(c)) => c
            .arg
            .as_deref()
            .is_none_or(|a| collect(a, scope, out, count)),
        Some(node::Node::NamedArgExpr(a)) => a
            .arg
            .as_deref()
            .is_none_or(|a| collect(a, scope, out, count)),
        Some(node::Node::NullTest(t)) => t
            .arg
            .as_deref()
            .is_none_or(|a| collect(a, scope, out, count)),
        Some(node::Node::BooleanTest(t)) => t
            .arg
            .as_deref()
            .is_none_or(|a| collect(a, scope, out, count)),
        _ => true,
    }
}

impl Subst<'_> {
    pub(crate) fn eval(&self, n: &protobuf::Node) -> Val {
        let Some(inner) = n.node.as_ref() else {
            return Val::Unknown;
        };
        match inner {
            node::Node::ColumnRef(_) => match plain_column(n, self.scope) {
                Some((c, _)) if &c == self.null => Val::Null,
                _ => Val::Unknown,
            },
            node::Node::AConst(c) => {
                if c.isnull {
                    return Val::Null;
                }
                match c.val.as_ref() {
                    Some(ConstVal::Ival(i)) => Val::Int(i.ival as i64),
                    Some(ConstVal::Sval(s)) => Val::Text(s.sval.clone()),
                    Some(ConstVal::Boolval(b)) => Val::Bool(b.boolval),
                    Some(_) => Val::NonNull,
                    None => Val::Unknown,
                }
            }
            node::Node::TypeCast(tc) => {
                let v = tc.arg.as_deref().map_or(Val::Unknown, |a| self.eval(a));
                if v == Val::Null {
                    // A strict cast (or one with no function) of NULL.
                    return if self.log.is_strict(tc.location, StrictNode::Cast) {
                        Val::Null
                    } else {
                        Val::Unknown
                    };
                }
                // Not through a type modifier, which may change the value
                // (`15::numeric(1,-1)` is 20).
                let target = super::cast_target(tc, self.snapshot);
                use crate::pg_catalog::oid;
                match (v, target) {
                    (Val::Int(i), Some(t))
                        if [oid::INT2, oid::INT4, oid::INT8, oid::NUMERIC].contains(&t) =>
                    {
                        Val::Int(i)
                    }
                    (Val::Bool(b), Some(t)) if t == oid::BOOL => Val::Bool(b),
                    (Val::Text(s), Some(t)) if t == oid::BOOL => {
                        match s.trim().to_ascii_lowercase().as_str() {
                            "t" | "true" => Val::Bool(true),
                            "f" | "false" => Val::Bool(false),
                            _ => Val::Unknown,
                        }
                    }
                    (Val::Text(s), Some(t)) if t == oid::TEXT => Val::Text(s),
                    _ => Val::Unknown,
                }
            }
            node::Node::NullTest(t) => {
                let v = t.arg.as_deref().map_or(Val::Unknown, |a| self.eval(a));
                let is_null = match v {
                    Val::Null => true,
                    v if v.known_non_null() => false,
                    _ => return Val::Unknown,
                };
                match protobuf::NullTestType::try_from(t.nulltesttype) {
                    Ok(protobuf::NullTestType::IsNull) => Val::Bool(is_null),
                    Ok(protobuf::NullTestType::IsNotNull) => Val::Bool(!is_null),
                    _ => Val::Unknown,
                }
            }
            node::Node::BooleanTest(t) => {
                use protobuf::BoolTestType as B;
                let v = t.arg.as_deref().map_or(Val::Unknown, |a| self.eval(a));
                let v = match v {
                    Val::Null => None,
                    Val::Bool(b) => Some(b),
                    _ => return Val::Unknown,
                };
                Val::Bool(match B::try_from(t.booltesttype) {
                    Ok(B::IsTrue) => v == Some(true),
                    Ok(B::IsNotTrue) => v != Some(true),
                    Ok(B::IsFalse) => v == Some(false),
                    Ok(B::IsNotFalse) => v != Some(false),
                    Ok(B::IsUnknown) => v.is_none(),
                    Ok(B::IsNotUnknown) => v.is_some(),
                    _ => return Val::Unknown,
                })
            }
            node::Node::BoolExpr(b) => {
                let vals: Vec<Val> = b.args.iter().map(|a| self.eval(a)).collect();
                match protobuf::BoolExprType::try_from(b.boolop) {
                    Ok(protobuf::BoolExprType::NotExpr) => match vals.as_slice() {
                        [Val::Null] => Val::Null,
                        [Val::Bool(v)] => Val::Bool(!v),
                        _ => Val::Unknown,
                    },
                    Ok(protobuf::BoolExprType::AndExpr) => three_valued(&vals, false),
                    Ok(protobuf::BoolExprType::OrExpr) => three_valued(&vals, true),
                    _ => Val::Unknown,
                }
            }
            node::Node::CoalesceExpr(c) => {
                for a in &c.args {
                    match self.eval(a) {
                        Val::Null => continue,
                        v => return self.converted(c.location, v),
                    }
                }
                Val::Null
            }
            node::Node::MinMaxExpr(m) => {
                let greatest =
                    protobuf::MinMaxOp::try_from(m.op) == Ok(protobuf::MinMaxOp::IsGreatest);
                let mut ints = Vec::new();
                let mut non_null = false;
                let mut unknown = false;
                for a in &m.args {
                    match self.eval(a) {
                        Val::Null => {}
                        Val::Int(i) => ints.push(i),
                        Val::Unknown => unknown = true,
                        _ => non_null = true,
                    }
                }
                if unknown {
                    // An unknown argument may be the one that wins.
                    if non_null || !ints.is_empty() {
                        Val::NonNull
                    } else {
                        Val::Unknown
                    }
                } else if non_null {
                    Val::NonNull
                } else if ints.is_empty() {
                    Val::Null
                } else if greatest {
                    self.converted(m.location, Val::Int(*ints.iter().max().expect("non-empty")))
                } else {
                    self.converted(m.location, Val::Int(*ints.iter().min().expect("non-empty")))
                }
            }
            node::Node::AExpr(e) => self.eval_a_expr(e),
            node::Node::FuncCall(f) => self.eval_func(f),
            _ => Val::Unknown,
        }
    }

    /// A constant `v` as the COALESCE / GREATEST / LEAST at `location`
    /// yields it, converted to its common type: as is when the conversion
    /// keeps it ([`folds_exactly`]), else just some non-NULL value.
    fn converted(&self, location: i32, v: Val) -> Val {
        if self.log.is_strict(location, StrictNode::ExactFold) {
            return v;
        }
        match v {
            Val::Int(i)
                if i.abs() <= FLOAT_EXACT
                    && self.log.is_strict(location, StrictNode::FloatFold) =>
            {
                v
            }
            Val::Int(_) | Val::Text(_) | Val::Bool(_) => Val::NonNull,
            v => v,
        }
    }

    /// Whether integer constants `a` and `b` compare as written under the
    /// comparison at `location`: a built-in one over exact types, or over
    /// a float type with both within its precision.
    fn ints_compare(&self, location: i32, a: i64, b: i64) -> bool {
        self.log.is_strict(location, StrictNode::StdCompare)
            || (self.log.is_strict(location, StrictNode::FloatCompare)
                && a.abs() <= FLOAT_EXACT
                && b.abs() <= FLOAT_EXACT)
    }

    fn eval_a_expr(&self, e: &protobuf::AExpr) -> Val {
        use protobuf::AExprKind as K;
        let l = e.lexpr.as_deref().map(|n| self.eval(n));
        let r = e.rexpr.as_deref().map(|n| self.eval(n));
        let strict = self.log.is_strict(e.location, StrictNode::Op);
        match K::try_from(e.kind) {
            Ok(K::AexprOp) => {
                if !strict {
                    return Val::Unknown;
                }
                if l.as_ref() == Some(&Val::Null) || r.as_ref() == Some(&Val::Null) {
                    return Val::Null;
                }
                let op = crate::expr::extract_string_fields(&e.name).join(".");
                match (l, r) {
                    (Some(l), Some(r)) => self.compare(e.location, &op, &l, &r),
                    _ => Val::Unknown,
                }
            }
            // `x IN (…)`, `x BETWEEN …`, `x op ANY (array)` with a NULL `x`
            // and strict comparisons: NULL (an IN list, a BETWEEN's bounds
            // are never empty; an empty array makes ANY FALSE, ALL TRUE).
            Ok(
                K::AexprIn
                | K::AexprBetween
                | K::AexprNotBetween
                | K::AexprBetweenSym
                | K::AexprNotBetweenSym,
            ) if strict && l == Some(Val::Null) => Val::Null,
            Ok(K::AexprNullif) => match (l, r) {
                (Some(Val::Null), _) => Val::Null,
                (Some(Val::Int(a)), Some(Val::Int(b))) if self.ints_compare(e.location, a, b) => {
                    if a == b {
                        Val::Null
                    } else {
                        Val::Int(a)
                    }
                }
                (Some(Val::Int(a)), Some(Val::Null)) => Val::Int(a),
                _ => Val::Unknown,
            },
            Ok(K::AexprDistinct | K::AexprNotDistinct) => {
                let distinct = K::try_from(e.kind) == Ok(K::AexprDistinct);
                let same = match (l, r) {
                    (Some(Val::Null), Some(Val::Null)) => true,
                    (Some(Val::Null), Some(v)) | (Some(v), Some(Val::Null))
                        if v.known_non_null() =>
                    {
                        false
                    }
                    (Some(Val::Int(a)), Some(Val::Int(b)))
                        if self.ints_compare(e.location, a, b) =>
                    {
                        a == b
                    }
                    (Some(a), Some(b))
                        if self.log.is_strict(e.location, StrictNode::StdCompare) =>
                    {
                        match equal_values(&a, &b) {
                            Some(eq) => eq,
                            None => return Val::Unknown,
                        }
                    }
                    _ => return Val::Unknown,
                };
                Val::Bool(same != distinct)
            }
            _ => Val::Unknown,
        }
    }

    /// `l op r` over two non-NULL operands, for a built-in comparison.
    fn compare(&self, location: i32, op: &str, l: &Val, r: &Val) -> Val {
        if l.known_non_null() && r.known_non_null() {
            let numeric = self.log.is_strict(location, StrictNode::StdCompare);
            let text = self.log.is_strict(location, StrictNode::TextEquality);
            let ord = match (l, r) {
                (Val::Int(a), Val::Int(b)) if self.ints_compare(location, *a, *b) => Some(a.cmp(b)),
                (Val::Bool(a), Val::Bool(b)) if numeric && matches!(op, "=" | "<>") => {
                    Some(a.cmp(b))
                }
                (Val::Text(a), Val::Text(b)) if text && matches!(op, "=" | "<>") => Some(a.cmp(b)),
                _ => None,
            };
            if let Some(ord) = ord {
                use std::cmp::Ordering::*;
                return Val::Bool(match op {
                    "=" => ord == Equal,
                    "<>" | "!=" => ord != Equal,
                    "<" => ord == Less,
                    ">" => ord == Greater,
                    "<=" => ord != Greater,
                    ">=" => ord != Less,
                    _ => return Val::Unknown,
                });
            }
        }
        Val::Unknown
    }

    fn eval_func(&self, f: &protobuf::FuncCall) -> Val {
        let vals: Vec<Val> = f.args.iter().map(|a| self.eval(a)).collect();
        if self.log.is_strict(f.location, StrictNode::Func) {
            return if vals.contains(&Val::Null) {
                Val::Null
            } else {
                Val::Unknown
            };
        }
        if f.func_variadic || f.agg_filter.is_some() || f.over.is_some() || !f.agg_order.is_empty()
        {
            return Val::Unknown;
        }
        let parts = crate::expr::extract_string_fields(&f.funcname);
        let (schema, name) = match parts.as_slice() {
            [n] => (None, n.as_str()),
            [s, n] if s == "pg_catalog" => (Some("pg_catalog"), n.as_str()),
            _ => return Val::Unknown,
        };
        if !matches!(name, "concat" | "num_nulls" | "num_nonnulls") {
            return Val::Unknown;
        }
        // Only the built-in one: no other function of the name anywhere
        // on the path could win the call.
        let candidates = self.snapshot.find_functions(schema, name);
        if candidates.is_empty()
            || candidates
                .iter()
                .any(|p| self.snapshot.namespace_name(p.pronamespace) != Some("pg_catalog"))
        {
            return Val::Unknown;
        }
        match name {
            "num_nulls" | "num_nonnulls" => {
                let mut nulls = 0i64;
                for v in &vals {
                    match v {
                        Val::Null => nulls += 1,
                        v if v.known_non_null() => {}
                        _ => return Val::NonNull,
                    }
                }
                let n = if name == "num_nulls" {
                    nulls
                } else {
                    vals.len() as i64 - nulls
                };
                Val::Int(n)
            }
            // concat skips NULL arguments and is never NULL itself.
            _ => {
                let mut out = String::new();
                for v in &vals {
                    match v {
                        Val::Null => {}
                        Val::Text(s) => out.push_str(s),
                        Val::Int(i) => out.push_str(&i.to_string()),
                        _ => return Val::NonNull,
                    }
                }
                Val::Text(out)
            }
        }
    }
}

/// Whether two non-NULL constants are equal (`None`: not comparable here).
fn equal_values(a: &Val, b: &Val) -> Option<bool> {
    match (a, b) {
        (Val::Int(a), Val::Int(b)) => Some(a == b),
        (Val::Bool(a), Val::Bool(b)) => Some(a == b),
        _ => None,
    }
}

/// AND (`or == false`) / OR over three-valued operands.
fn three_valued(vals: &[Val], or: bool) -> Val {
    let mut null = false;
    let mut unknown = false;
    for v in vals {
        match v {
            Val::Bool(b) if *b == or => return Val::Bool(or),
            Val::Bool(_) => {}
            Val::Null => null = true,
            _ => unknown = true,
        }
    }
    if unknown {
        Val::Unknown
    } else if null {
        Val::Null
    } else {
        Val::Bool(!or)
    }
}
