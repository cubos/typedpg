//! Whether `ALTER TABLE … ATTACH PARTITION` scans the table it attaches.
//!
//! PostgreSQL skips the scan when the table's own constraints imply the
//! partition constraint (`QueuePartitionConstraintValidation` →
//! `PartConstraintImpliedByRelConstraint`, src/backend/commands/tablecmds.c):
//! the constraint is built from the bound (`get_qual_for_list` /
//! `get_qual_for_range`, src/backend/partitioning/partbounds.c), with the
//! parent's own partition constraint appended when the parent is itself a
//! partition, and proven with `predicate_implied_by(…, weak = true)`
//! (src/backend/optimizer/util/predtest.c) from the table's validated
//! `CHECK` constraints and an `IS NOT NULL` test for each column with a
//! valid NOT NULL constraint. A partitioned table whose constraints don't
//! imply it has each of its partitions checked the same way.
//!
//! This module rebuilds the partition constraint and runs a faithful subset
//! of predtest: the `AND` / `OR` recursion, `equal()` atoms, `x IS NOT NULL`
//! and `x IS NULL` tests, and the btree implication of `key op constant`
//! comparisons (`<`, `<=`, `=`, `>=`, `>`), an `IN` list or `= ANY (ARRAY
//! […])` (up to predtest's 100 elements) being an `OR` of equalities and
//! `BETWEEN` an `AND`. Whatever it can't model — an expression key, a key
//! whose operator class or collation differs from the column's, a constant
//! it can't order, a hash bound, a default partition's negated constraint —
//! is not proven, so the attach is taken to scan.

use std::cmp::Ordering;

use typedpg_pg_query::ast::{self, AExprKind, BoolExprType, NullTestType, PartitionBoundSpec};

use super::{Bound, Datum, PartKey, PartSpec, RangeDatum, Strategy, normalize, orderable};
use crate::oid::PgClassOid;
use crate::pg_catalog::{ConType, PgCatalog, RelKind};

/// predtest's `MAX_SAOP_ARRAY_SIZE`: a longer `IN` list / `= ANY (ARRAY…)`
/// is an opaque atom, proven only by an identical one.
const MAX_SAOP_ARRAY_SIZE: usize = 100;

/// Whether attaching `part` to `parent` with `spec` makes PostgreSQL scan
/// `part` (or one of its partitions) to check the bound. `true` whenever
/// the implication can't be shown.
pub(crate) fn attach_scans(
    interp: &PgCatalog,
    parent: PgClassOid,
    part: PgClassOid,
    spec: &PartitionBoundSpec,
) -> bool {
    let Some(pspec) = interp.partition_specs.get(&parent) else {
        return true;
    };
    let Ok(bound) = super::transform_bound(interp, pspec, spec) else {
        return true;
    };
    // A list's duplicates are dropped from the bound: its texts only line
    // up with the values without them.
    let raws: Vec<Option<String>> = match &bound {
        Bound::List(values) if values.len() == spec.listdatums.len() => spec
            .listdatums
            .iter()
            .map(|v| literal_text(v.as_ref()))
            .collect(),
        _ => Vec::new(),
    };
    let raw_range = |values: &[Option<ast::Node>]| -> Vec<Option<String>> {
        values.iter().map(|v| literal_text(v.as_ref())).collect()
    };
    let raws = Raws {
        list: raws,
        lower: raw_range(&spec.lowerdatums),
        upper: raw_range(&spec.upperdatums),
    };
    let mut constraint = Vec::new();
    let Some(own) = bound_qual(interp, parent, Some(part), pspec, &bound, &raws) else {
        return true;
    };
    constraint.extend(own);
    // The parent's own partition constraint, and its parent's, …
    let mut child = parent;
    for _ in 0..64 {
        let Some(grandparent) = interp
            .pg_inherits
            .iter()
            .find(|i| i.inhrelid == child)
            .map(|i| i.inhparent)
        else {
            break;
        };
        let (Some(gspec), Some(gbound)) = (
            interp.partition_specs.get(&grandparent),
            interp.partition_bounds.get(&child),
        ) else {
            break;
        };
        let Some(qual) = bound_qual(
            interp,
            grandparent,
            Some(child),
            gspec,
            gbound,
            &Raws::default(),
        ) else {
            return true;
        };
        constraint.extend(qual);
        child = grandparent;
    }
    if constraint.is_empty() {
        return false;
    }
    // The key columns of every level, as the constraint compares them.
    let mut keys: Vec<(&str, &PartKey)> = Vec::new();
    let mut ancestor = Some(parent);
    for _ in 0..64 {
        let Some(relid) = ancestor else {
            break;
        };
        if let Some(spec) = interp.partition_specs.get(&relid) {
            for key in &spec.keys {
                if let Some(name) = &key.name
                    && !keys.iter().any(|(n, _)| n == name)
                {
                    keys.push((name, key));
                }
            }
        }
        ancestor = interp
            .pg_inherits
            .iter()
            .find(|i| i.inhrelid == relid)
            .map(|i| i.inhparent);
    }
    let predicate = Expr::And(constraint);
    !implied_for(interp, part, &predicate, &keys, 0)
}

/// Whether `relid`'s constraints imply `predicate`, or — for a partitioned
/// table — each partition's do.
fn implied_for(
    interp: &PgCatalog,
    relid: PgClassOid,
    predicate: &Expr,
    keys: &[(&str, &PartKey)],
    depth: usize,
) -> bool {
    if implies(&Expr::And(constraints(interp, relid, keys)), predicate) {
        return true;
    }
    match interp.pg_class.get(&relid).map(|c| c.relkind) {
        Some(RelKind::Partitioned) if depth < 32 => super::inherit::children_of(interp, relid)
            .into_iter()
            .all(|child| implied_for(interp, child, predicate, keys, depth + 1)),
        _ => false,
    }
}

/// The literal text of each bound value, where it is one: two equal texts
/// coerced to one type are equal, even when the analyzer can't compute
/// the value.
#[derive(Default)]
struct Raws {
    list: Vec<Option<String>>,
    lower: Vec<Option<String>>,
    upper: Vec<Option<String>>,
}

/// A predicate or constraint, as predtest sees it.
#[derive(Clone, Debug)]
enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    NotNull(String),
    IsNull(String),
    Cmp(String, Op, Val),
    /// Anything else: it implies nothing, and nothing implies it.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

impl Op {
    fn from_name(name: &str) -> Option<Op> {
        Some(match name {
            "<" => Op::Lt,
            "<=" => Op::Le,
            "=" => Op::Eq,
            ">=" => Op::Ge,
            ">" => Op::Gt,
            _ => return None,
        })
    }

    /// The operator with its operands swapped (`5 < x` is `x > 5`).
    fn commuted(self) -> Op {
        match self {
            Op::Lt => Op::Gt,
            Op::Le => Op::Ge,
            Op::Eq => Op::Eq,
            Op::Ge => Op::Le,
            Op::Gt => Op::Lt,
        }
    }
}

/// A constant compared with a key column.
#[derive(Clone, Debug)]
struct Val {
    datum: Datum,
    /// The literal as written, when it is one.
    raw: Option<String>,
}

impl Val {
    /// Their order under the key's comparison, when the analyzer knows it.
    fn cmp(&self, other: &Val) -> Option<Ordering> {
        if orderable(&self.datum)
            && std::mem::discriminant(&self.datum) == std::mem::discriminant(&other.datum)
        {
            return Some(self.datum.cmp(&other.datum));
        }
        if self.datum != Datum::Opaque && self.datum == other.datum {
            return Some(Ordering::Equal);
        }
        match (&self.raw, &other.raw) {
            (Some(a), Some(b)) if a == b => Some(Ordering::Equal),
            _ => None,
        }
    }
}

/// The partition constraint `bound` gives a partition of `parent`, as a
/// list of `AND`ed items: `None` when it can't be modeled.
fn bound_qual(
    interp: &PgCatalog,
    parent: PgClassOid,
    part: Option<PgClassOid>,
    spec: &PartSpec,
    bound: &Bound,
    raws: &Raws,
) -> Option<Vec<Expr>> {
    let column = |key: &PartKey| key.name.clone();
    match bound {
        Bound::Hash { .. } => None,
        Bound::Default => {
            // The negation of every other partition's constraint: nothing
            // to check while there is no other partition, and not modeled
            // otherwise.
            let others = super::inherit::children_of(interp, parent)
                .into_iter()
                .filter(|&c| Some(c) != part)
                .any(|c| {
                    interp
                        .partition_bounds
                        .get(&c)
                        .is_some_and(|b| !matches!(b, Bound::Default))
                });
            (!others).then(Vec::new)
        }
        Bound::List(values) => {
            let col = column(spec.keys.first()?)?;
            let has_null = values.iter().any(Option::is_none);
            let elems: Vec<Expr> = values
                .iter()
                .enumerate()
                .filter_map(|(i, v)| {
                    let datum = v.clone()?;
                    Some(Expr::Cmp(
                        col.clone(),
                        Op::Eq,
                        Val {
                            datum,
                            raw: raws.list.get(i).cloned().flatten(),
                        },
                    ))
                })
                .collect();
            if elems.len() > MAX_SAOP_ARRAY_SIZE {
                return None;
            }
            let any = (!elems.is_empty()).then_some(Expr::Or(elems));
            Some(if has_null {
                vec![match any {
                    Some(any) => Expr::Or(vec![Expr::IsNull(col), any]),
                    None => Expr::IsNull(col),
                }]
            } else {
                let mut items = vec![Expr::NotNull(col)];
                items.extend(any);
                items
            })
        }
        Bound::Range { lower, upper } => {
            if spec.strategy != Strategy::Range {
                return None;
            }
            let cols: Vec<String> = spec.keys.iter().map(column).collect::<Option<_>>()?;
            let n = cols.len();
            let value =
                |datums: &[RangeDatum], raws: &[Option<String>], i: usize| match datums.get(i) {
                    Some(RangeDatum::Value(d)) => Some(Val {
                        datum: d.clone(),
                        raw: raws.get(i).cloned().flatten(),
                    }),
                    _ => None,
                };
            let is_value = |d: Option<&RangeDatum>| matches!(d, Some(RangeDatum::Value(_)));
            // get_range_nulltest: every key column IS NOT NULL.
            let mut items: Vec<Expr> = cols.iter().cloned().map(Expr::NotNull).collect();
            // The leading columns whose lower and upper bounds are equal.
            let mut i = 0;
            while i < n {
                let (Some(lo), Some(hi)) =
                    (value(lower, &raws.lower, i), value(upper, &raws.upper, i))
                else {
                    break;
                };
                // The last column's bounds can't be equal (the range would
                // be empty, which PostgreSQL rejects), even where the
                // analyzer can't compare them.
                if i == n - 1 || lo.cmp(&hi)? != Ordering::Equal {
                    break;
                }
                items.push(Expr::Cmp(cols[i].clone(), Op::Eq, lo));
                i += 1;
            }
            let mut lower_arms = Vec::new();
            let mut upper_arms = Vec::new();
            let (mut need_lower, mut need_upper) = (true, true);
            let mut current = 0;
            while current < n - i {
                let mut lower_args = Vec::new();
                let mut upper_args = Vec::new();
                let mut j = i;
                while j < n {
                    let lo = value(lower, &raws.lower, j);
                    let hi = value(upper, &raws.upper, j);
                    let lower_next = lower.get(j + 1);
                    let upper_next = upper.get(j + 1);
                    if need_lower && let Some(lo) = &lo {
                        let op = if j - i < current {
                            Op::Eq
                        } else if j == n - 1 || matches!(lower_next, Some(RangeDatum::MinValue)) {
                            Op::Ge
                        } else {
                            Op::Gt
                        };
                        lower_args.push(Expr::Cmp(cols[j].clone(), op, lo.clone()));
                    }
                    if need_upper && let Some(hi) = &hi {
                        let op = if j - i < current {
                            Op::Eq
                        } else if matches!(upper_next, Some(RangeDatum::MaxValue)) {
                            Op::Le
                        } else {
                            Op::Lt
                        };
                        upper_args.push(Expr::Cmp(cols[j].clone(), op, hi.clone()));
                    }
                    j += 1;
                    if j - i > current {
                        if lo.is_none() || !is_value(lower_next) {
                            need_lower = false;
                        }
                        if hi.is_none() || !is_value(upper_next) {
                            need_upper = false;
                        }
                        break;
                    }
                }
                if !lower_args.is_empty() {
                    lower_arms.push(and_or_single(lower_args));
                }
                if !upper_args.is_empty() {
                    upper_arms.push(and_or_single(upper_args));
                }
                if !need_lower && !need_upper {
                    break;
                }
                current += 1;
            }
            if !lower_arms.is_empty() {
                items.push(or_or_single(lower_arms));
            }
            if !upper_arms.is_empty() {
                items.push(or_or_single(upper_arms));
            }
            Some(items)
        }
    }
}

fn and_or_single(mut items: Vec<Expr>) -> Expr {
    if items.len() == 1 {
        items.remove(0)
    } else {
        Expr::And(items)
    }
}

fn or_or_single(mut items: Vec<Expr>) -> Expr {
    if items.len() == 1 {
        items.remove(0)
    } else {
        Expr::Or(items)
    }
}

/// What PartConstraintImpliedByRelConstraint proves from: an `IS NOT
/// NULL` test of each column with a valid NOT NULL constraint, and each
/// validated, enforced `CHECK` constraint.
fn constraints(interp: &PgCatalog, relid: PgClassOid, keys: &[(&str, &PartKey)]) -> Vec<Expr> {
    let mut out = Vec::new();
    for attribute in interp.attributes_of(relid) {
        let invalid = interp.pg_constraint.values().any(|c| {
            c.conrelid == relid
                && c.contype == ConType::NotNull
                && c.conkey == [attribute.attnum]
                && !c.convalidated
        });
        if attribute.attnotnull && !invalid {
            out.push(Expr::NotNull(attribute.attname.clone()));
        }
    }
    // The key columns the CHECK constraints can be compared on: the key
    // must order as the column's own comparison does (its type's default
    // btree operator class, under the column's collation).
    let comparable: Vec<(&str, &PartKey)> = keys
        .iter()
        .filter(|(name, key)| {
            interp.attribute_by_name(relid, name).is_some_and(|a| {
                a.atttypid == key.type_oid
                    && a.attcollation == key.collation
                    && key.builtin_order
                    && key.opclass.is_some_and(|oid| {
                        crate::ddl::opclass::opclass_by_oid(interp, oid)
                            .is_some_and(|c| c.opcdefault && c.opcmethod == "btree")
                    })
            })
        })
        .copied()
        .collect();
    let mut checks: Vec<_> = interp
        .pg_constraint
        .values()
        .filter(|c| {
            c.conrelid == relid && c.contype == ConType::Check && c.convalidated && c.conenforced
        })
        .collect();
    checks.sort_by_key(|c| c.oid);
    for check in checks {
        let Some(def) = interp.check_defs.get(&check.oid) else {
            continue;
        };
        let crate::ddl::tables::check_inherit::StoredExpr::Written(expr) = &def.expr else {
            continue;
        };
        out.push(check_expr(interp, (**expr).as_ref(), &comparable));
    }
    out
}

/// A `CHECK` expression as predtest reads it.
fn check_expr(interp: &PgCatalog, node: Option<&ast::Node>, keys: &[(&str, &PartKey)]) -> Expr {
    let Some(node) = node else {
        return Expr::Other;
    };
    match node {
        ast::Node::BoolExpr(b) => {
            let args = || {
                b.args
                    .iter()
                    .map(|a| check_expr(interp, a.as_ref(), keys))
                    .collect()
            };
            match b.boolop {
                BoolExprType::AndExpr => Expr::And(args()),
                BoolExprType::OrExpr => Expr::Or(args()),
                BoolExprType::NotExpr => Expr::Other,
            }
        }
        ast::Node::NullTest(t) => match column_name(t.arg.as_ref()) {
            Some(col) if !t.argisrow => match t.nulltesttype {
                NullTestType::IsNotNull => Expr::NotNull(col.to_owned()),
                NullTestType::IsNull => Expr::IsNull(col.to_owned()),
            },
            _ => Expr::Other,
        },
        ast::Node::AExpr(a) => aexpr(interp, a, keys).unwrap_or(Expr::Other),
        _ => Expr::Other,
    }
}

fn aexpr(interp: &PgCatalog, a: &ast::AExpr, keys: &[(&str, &PartKey)]) -> Option<Expr> {
    let compared = |column: Option<&ast::Node>, value: Option<&ast::Node>, op: Op| {
        let col = column_name(column)?;
        let (_, key) = keys.iter().find(|(name, _)| *name == col)?;
        Some(Expr::Cmp(
            col.to_owned(),
            op,
            check_value(interp, value?, key)?,
        ))
    };
    // `BETWEEN` (named so) is `>= AND <=`; the others name their operator.
    if a.kind == AExprKind::AexprBetween {
        let Some(ast::Node::List(list)) = a.rexpr.as_ref() else {
            return None;
        };
        let [low, high] = list.items.as_slice() else {
            return None;
        };
        return Some(Expr::And(vec![
            compared(a.lexpr.as_ref(), low.as_ref(), Op::Ge)?,
            compared(a.lexpr.as_ref(), high.as_ref(), Op::Le)?,
        ]));
    }
    let op = operator(&a.name)?;
    match a.kind {
        AExprKind::AexprOp => compared(a.lexpr.as_ref(), a.rexpr.as_ref(), op)
            .or_else(|| compared(a.rexpr.as_ref(), a.lexpr.as_ref(), op.commuted())),
        AExprKind::AexprIn if op == Op::Eq => {
            let Some(ast::Node::List(list)) = a.rexpr.as_ref() else {
                return None;
            };
            equalities(&list.items, |v| compared(a.lexpr.as_ref(), v, Op::Eq))
        }
        AExprKind::AexprOpAny if op == Op::Eq => {
            let Some(ast::Node::AArrayExpr(array)) = a.rexpr.as_ref() else {
                return None;
            };
            equalities(&array.elements, |v| compared(a.lexpr.as_ref(), v, Op::Eq))
        }
        _ => None,
    }
}

/// An `IN` list's or array's elements as an `OR` of equalities.
fn equalities(
    elements: &[Option<ast::Node>],
    mut equality: impl FnMut(Option<&ast::Node>) -> Option<Expr>,
) -> Option<Expr> {
    if elements.is_empty() || elements.len() > MAX_SAOP_ARRAY_SIZE {
        return None;
    }
    let arms = elements
        .iter()
        .map(|e| equality(e.as_ref()))
        .collect::<Option<Vec<_>>>()?;
    Some(Expr::Or(arms))
}

/// The comparison an `A_Expr` name is: one of the built-in btree
/// operators, unqualified or in `pg_catalog`.
fn operator(name: &[Option<ast::Node>]) -> Option<Op> {
    let names: Vec<&str> = name
        .iter()
        .filter_map(|n| crate::ddl::util::node_string(n.as_ref()))
        .collect();
    match names.as_slice() {
        [op] | ["pg_catalog", op] => Op::from_name(op),
        _ => None,
    }
}

/// The column a plain one-name column reference names.
fn column_name(node: Option<&ast::Node>) -> Option<&str> {
    let Some(ast::Node::ColumnRef(c)) = node else {
        return None;
    };
    match c.fields.as_slice() {
        [field] => crate::ddl::util::node_string(field.as_ref()),
        _ => None,
    }
}

/// A constant a `CHECK` compares a key column with, as the key orders it —
/// only when PostgreSQL compares it with the column itself, under an
/// operator of the key's family: an untyped literal (coerced to the
/// column's type), an integer literal for an integer key, or a literal cast
/// to the key's type. A constant that makes PostgreSQL cast the column
/// instead (`int_col >= 1.5` compares numerics) is not one.
fn check_value(interp: &PgCatalog, v: &ast::Node, key: &PartKey) -> Option<Val> {
    use crate::pg_catalog::oid;
    let base = interp.unwrap_domain(key.type_oid);
    let integer_key = [oid::INT2, oid::INT4, oid::INT8].contains(&base);
    let (literal, cast) = match v {
        ast::Node::AConst(c) if !c.isnull => (c, None),
        ast::Node::TypeCast(tc) => match tc.arg.as_ref() {
            Some(ast::Node::AConst(c)) if !c.isnull => {
                let target =
                    crate::nonnull::cast_target(tc, interp).map(|t| interp.unwrap_domain(t))?;
                (c, Some(target))
            }
            _ => return None,
        },
        _ => return None,
    };
    // A cast to another type is only compared with the column as is when
    // both are integers (one btree family).
    if let Some(target) = cast
        && target != base
        && !(integer_key && [oid::INT2, oid::INT4, oid::INT8].contains(&target))
    {
        return None;
    }
    let raw = literal_text(Some(v))?;
    if integer_key {
        let value = match literal.val.as_ref()? {
            ast::AConstValue::Integer(i) => i128::from(i.ival),
            // An integer literal past int4's range is an int8 constant;
            // one with a fraction or an exponent is numeric.
            ast::AConstValue::Float(f) => f.fval.parse::<i128>().ok()?,
            ast::AConstValue::String(s) => crate::literal_input::parse_pg_integer(&s.sval)?,
            _ => return None,
        };
        return Some(Val {
            datum: Datum::Int(value),
            raw: Some(raw),
        });
    }
    // Any other key: an untyped literal or one cast to the key's type is
    // read by the type's input function; a numeric constant is compared
    // as a numeric / float key's value, TRUE / FALSE as a boolean key's.
    // The column's type modifier is not
    // applied to a CHECK's constant.
    match literal.val.as_ref()? {
        ast::AConstValue::String(_) => {}
        ast::AConstValue::Integer(_) | ast::AConstValue::Float(_)
            if [oid::NUMERIC, oid::FLOAT4, oid::FLOAT8].contains(&base) && cast.is_none() => {}
        ast::AConstValue::Boolean(_) if base == oid::BOOL => {}
        _ => return None,
    }
    let unmodified = PartKey {
        typmod: None,
        ..key.clone()
    };
    Some(Val {
        datum: normalize(interp, v, &unmodified, interp.timezone_utc.get()),
        raw: Some(raw),
    })
}

/// A literal's text (`'2024-01-01'`, `1000`, `'x'::text`'s `x`), the cast
/// left out.
fn literal_text(node: Option<&ast::Node>) -> Option<String> {
    let constant = match node? {
        ast::Node::AConst(c) => c,
        ast::Node::TypeCast(tc) => match tc.arg.as_ref()? {
            ast::Node::AConst(c) => c,
            _ => return None,
        },
        _ => return None,
    };
    if constant.isnull {
        return None;
    }
    Some(match constant.val.as_ref()? {
        ast::AConstValue::Integer(i) => i.ival.to_string(),
        ast::AConstValue::Float(f) => f.fval.clone(),
        ast::AConstValue::String(s) => s.sval.clone(),
        ast::AConstValue::Boolean(b) => b.boolval.to_string(),
        ast::AConstValue::BitString(b) => b.bsval.clone(),
    })
}

/// predicate_implied_by(predicate, clause, weak = true): whenever `clause`
/// is not false, neither is `predicate`.
fn implies(clause: &Expr, predicate: &Expr) -> bool {
    match (clause, predicate) {
        (_, Expr::And(items)) => {
            items.iter().all(|p| implies(clause, p))
                || matches!(clause, Expr::Or(arms) if !arms.is_empty()
                    && arms.iter().all(|c| implies(c, predicate)))
        }
        (Expr::And(items), _) => {
            items.iter().any(|c| implies(c, predicate))
                || matches!(predicate, Expr::Or(arms) if arms.iter().any(|p| implies(clause, p)))
        }
        (Expr::Or(arms), _) => !arms.is_empty() && arms.iter().all(|c| implies(c, predicate)),
        (_, Expr::Or(arms)) => arms.iter().any(|p| implies(clause, p)),
        (clause, predicate) => atom_implies(clause, predicate),
    }
}

/// predicate_implied_by_simple_clause, for the atoms modeled.
fn atom_implies(clause: &Expr, predicate: &Expr) -> bool {
    match (clause, predicate) {
        (Expr::NotNull(a), Expr::NotNull(b)) | (Expr::IsNull(a), Expr::IsNull(b)) => a == b,
        (Expr::Cmp(a, clause_op, c), Expr::Cmp(b, pred_op, p)) if a == b => {
            // The clause's values are a subset of the predicate's.
            let Some(order) = c.cmp(p) else {
                return false;
            };
            use Op::*;
            match (pred_op, clause_op) {
                (Eq, Eq) => order == Ordering::Equal,
                (Ge, Eq | Gt | Ge) => order != Ordering::Less,
                (Gt, Eq | Ge) => order == Ordering::Greater,
                (Gt, Gt) => order != Ordering::Less,
                (Le, Eq | Lt | Le) => order != Ordering::Greater,
                (Lt, Eq | Le) => order == Ordering::Less,
                (Lt, Lt) => order != Ordering::Greater,
                _ => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(i: i128) -> Val {
        Val {
            datum: Datum::Int(i),
            raw: None,
        }
    }

    fn cmp(op: Op, i: i128) -> Expr {
        Expr::Cmp("k".to_owned(), op, int(i))
    }

    #[test]
    fn btree_implication_matches_predtest() {
        use Op::*;
        // (clause, predicate, implied)
        for (clause, predicate, implied) in [
            (cmp(Eq, 5), cmp(Ge, 5), true),
            (cmp(Eq, 5), cmp(Gt, 5), false),
            (cmp(Gt, 5), cmp(Ge, 5), true),
            (cmp(Ge, 5), cmp(Gt, 5), false),
            (cmp(Ge, 6), cmp(Gt, 5), true),
            (cmp(Gt, 0), cmp(Ge, 1000), false),
            (cmp(Lt, 200), cmp(Lt, 200), true),
            (cmp(Le, 199), cmp(Lt, 200), true),
            (cmp(Le, 200), cmp(Lt, 200), false),
            (cmp(Lt, 100), cmp(Le, 100), true),
            (cmp(Ge, 5), cmp(Eq, 5), false),
            (cmp(Lt, 5), cmp(Gt, 1), false),
        ] {
            assert_eq!(
                implies(&clause, &predicate),
                implied,
                "{clause:?} => {predicate:?}"
            );
        }
    }

    #[test]
    fn is_not_null_is_not_weakly_implied_by_a_comparison() {
        let clause = Expr::And(vec![cmp(Op::Ge, 100), cmp(Op::Lt, 200)]);
        let predicate = Expr::And(vec![
            Expr::NotNull("k".to_owned()),
            cmp(Op::Ge, 100),
            cmp(Op::Lt, 200),
        ]);
        assert!(!implies(&clause, &predicate));
        let clause = Expr::And(vec![
            Expr::NotNull("k".to_owned()),
            Expr::And(vec![cmp(Op::Ge, 100), cmp(Op::Lt, 200)]),
        ]);
        assert!(implies(&clause, &predicate));
    }

    #[test]
    fn an_in_list_implies_a_wider_list() {
        let list = |values: &[i128]| Expr::Or(values.iter().map(|&v| cmp(Op::Eq, v)).collect());
        assert!(implies(&list(&[1, 2]), &list(&[1, 2, 3])));
        assert!(!implies(&list(&[1, 4]), &list(&[1, 2, 3])));
    }
}
