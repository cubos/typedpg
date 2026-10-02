//! What a HAVING clause proves of the groups it keeps, and how the builtin
//! aggregates behave over rows lacking values — what decides whether an
//! aggregate's result can be NULL.

use typedpg_pg_query::protobuf::{self, node};

use crate::expr;
use crate::nonnull::Col;
use crate::pg_catalog::PgCatalog;
use crate::scope::Scope;

/// What a HAVING clause proves of every group (or, without GROUP BY, the
/// single aggregate row) it lets through.
#[derive(Debug, Default)]
pub(crate) struct HavingFacts {
    /// The group has rows: HAVING is FALSE or NULL over an empty input
    /// (`count(*) > 0`, `max(a) > 0`, `NOT (count(*) <= 0)`, …).
    pub input_not_empty: bool,
    /// Columns some row of the group has non-NULL: HAVING is FALSE or
    /// NULL when none has (`count(b) > 0`, `sum(b) > 0`).
    pub nonnull_inputs: Vec<Col>,
}

/// How a builtin aggregate behaves over rows lacking a value, by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggregateClass {
    /// `count`, `regr_count`: 0 over no (non-NULL) input, never NULL.
    Count,
    /// The hypothetical-set `rank` / `dense_rank` / `percent_rank` /
    /// `cume_dist`: the hypothetical row is always ranked, never NULL.
    Hypothetical,
    /// Non-strict transition functions keeping (or skipping) NULL inputs
    /// but always building a state: NULL only over no rows at all.
    NullKeeping,
    /// The rest: NULL over no rows, and over rows whose aggregated values
    /// are all NULL (`max`, `sum`, `string_agg`'s value, `percentile_cont`'s
    /// ordering value, `xmlagg`, `range_agg`, …).
    Strict,
}

/// The class of the builtin aggregate `name` (see [`AggregateClass`]).
/// Taken from `pg_aggregate` on PostgreSQL 18 (transition function
/// strictness, initial condition, final function) and confirmed there over
/// an empty, an all-NULL and a non-NULL input for each.
pub(crate) fn aggregate_class(name: &str) -> AggregateClass {
    match name {
        "count" | "regr_count" => AggregateClass::Count,
        "rank" | "dense_rank" | "percent_rank" | "cume_dist" => AggregateClass::Hypothetical,
        "array_agg"
        | "json_agg"
        | "json_agg_strict"
        | "jsonb_agg"
        | "jsonb_agg_strict"
        | "json_object_agg"
        | "json_object_agg_strict"
        | "json_object_agg_unique"
        | "json_object_agg_unique_strict"
        | "jsonb_object_agg"
        | "jsonb_object_agg_strict"
        | "jsonb_object_agg_unique"
        | "jsonb_object_agg_unique_strict" => AggregateClass::NullKeeping,
        _ => AggregateClass::Strict,
    }
}

/// The name of the aggregate a plain call (no OVER) refers to, when every
/// routine of that name it can reach is a `pg_catalog` aggregate — so the
/// call is to the builtin whatever its argument types.
pub(crate) fn builtin_aggregate_name<'a>(
    fc: &'a protobuf::FuncCall,
    snapshot: &PgCatalog,
) -> Option<&'a str> {
    if fc.over.is_some() {
        return None;
    }
    let (schema, name) = match fc.funcname.as_slice() {
        [n] => (None, n),
        [s, n] => (Some(string_of(s)?), n),
        _ => return None,
    };
    let name = string_of(name)?;
    if schema.is_some_and(|s| s != "pg_catalog") {
        return None;
    }
    let catalog = snapshot.pg_catalog_oid()?;
    let procs = snapshot.find_functions(schema, name);
    (!procs.is_empty()
        && procs.iter().all(|p| {
            p.pronamespace == catalog && p.prokind == crate::pg_catalog::ProKind::Aggregate
        }))
    .then_some(name)
}

/// Whether an aggregate call belongs to the query level of `scope`: PG
/// assigns it to the innermost level of the columns it reads, so one over
/// outer columns only (`count(outer.x)`) is the outer query's — its value
/// says nothing of this level's groups. A call holding a subquery is not
/// looked into.
fn is_current_level(fc_node: &protobuf::Node, scope: &Scope) -> bool {
    let own = crate::nonnull::own_aliases(scope);
    let (mut refs, mut current, mut opaque) = (0, false, false);
    crate::resolve::visit_same_level(fc_node, &mut |e| match e.node.as_ref() {
        Some(node::Node::ColumnRef(_)) => {
            refs += 1;
            match column_of(e, scope) {
                Some((alias, _)) if own.contains(&alias) => current = true,
                Some(_) => {}
                None => opaque = true,
            }
        }
        Some(node::Node::SubLink(_)) => opaque = true,
        _ => {}
    });
    !opaque && (refs == 0 || current)
}

fn string_of(n: &protobuf::Node) -> Option<&str> {
    match n.node.as_ref()? {
        node::Node::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}

/// The values an aggregate reads (`string_agg`'s delimiter and an
/// ordered-set aggregate's direct arguments aside).
pub(crate) fn aggregated_args<'a>(
    fc: &'a protobuf::FuncCall,
    name: &str,
) -> Vec<&'a protobuf::Node> {
    if fc.agg_within_group {
        return fc
            .agg_order
            .iter()
            .filter_map(|o| match o.node.as_ref() {
                Some(node::Node::SortBy(sb)) => sb.node.as_deref(),
                _ => None,
            })
            .collect();
    }
    if name == "string_agg" {
        return fc.args.first().into_iter().collect();
    }
    fc.args.iter().collect()
}

/// The column a plain column reference resolves to.
pub(crate) fn column_of(n: &protobuf::Node, scope: &Scope) -> Option<Col> {
    let node::Node::ColumnRef(cr) = n.node.as_ref()? else {
        return None;
    };
    let parts = expr::extract_string_fields(&cr.fields);
    let (table, col) = match parts.as_slice() {
        [c] => (None, c.as_str()),
        [t, c] => (Some(t.as_str()), c.as_str()),
        _ => return None,
    };
    scope
        .resolve_column(table, col, None)
        .ok()
        .map(|c| (c.table_alias.clone(), c.name.clone()))
}

/// What HAVING proves: it is evaluated, three-valued, under the
/// hypothesis that the group is empty — every count 0, every other
/// builtin aggregate but the hypothetical-set ones NULL — and, for each
/// column an aggregate in it reads, that no row of the group has the
/// column non-NULL (its counts 0, the strict aggregates over it NULL).
/// A hypothesis under which HAVING can only be FALSE or NULL is refuted
/// for every group it keeps.
pub(crate) fn having_facts(
    having: &protobuf::Node,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> HavingFacts {
    let mut candidates: Vec<Col> = Vec::new();
    crate::resolve::visit_same_level(having, &mut |e| {
        if let Some(node::Node::FuncCall(fc)) = e.node.as_ref()
            && let Some(name) = builtin_aggregate_name(fc, snapshot)
            && is_current_level(e, scope)
        {
            for a in aggregated_args(fc, name) {
                if let Some(c) = column_of(a, scope)
                    && !candidates.contains(&c)
                {
                    candidates.push(c);
                }
            }
        }
    });
    let mut facts = HavingFacts {
        input_not_empty: eval(having, &Hypothesis::Empty, scope, snapshot).rejects(),
        nonnull_inputs: Vec::new(),
    };
    for col in candidates {
        if eval(having, &Hypothesis::AllNull(&col), scope, snapshot).rejects() {
            // A row with a non-NULL value is a row.
            facts.input_not_empty = true;
            facts.nonnull_inputs.push(col);
        }
    }
    facts
}

enum Hypothesis<'a> {
    /// The group has no rows.
    Empty,
    /// No row of the group has this column non-NULL.
    AllNull(&'a Col),
}

/// A HAVING sub-expression's possible values under a hypothesis.
#[derive(Debug, Clone, Copy)]
enum Val {
    /// Certainly NULL.
    Null,
    /// This number.
    Num(f64),
    /// A boolean among these outcomes (`T`, `F`, `N` bits).
    Bool(u8),
    /// Anything, NULL included.
    Any,
}

const T: u8 = 1;
const F: u8 = 2;
const N: u8 = 4;

impl Val {
    fn mask(self) -> u8 {
        match self {
            Val::Null => N,
            Val::Bool(m) => m,
            Val::Num(_) => T | F,
            Val::Any => T | F | N,
        }
    }

    /// Whether the qual can only be FALSE or NULL (the row is dropped).
    fn rejects(self) -> bool {
        self.mask() & T == 0
    }
}

/// Whether every operator named `name` is a strict `pg_catalog` one: then
/// a NULL operand makes the result NULL, and numbers compare as numbers.
fn builtin_strict_operator(name: &str, snapshot: &PgCatalog) -> bool {
    let Some(catalog) = snapshot.pg_catalog_oid() else {
        return false;
    };
    let mut any = false;
    for op in snapshot.pg_operator.values().filter(|o| o.oprname == name) {
        any = true;
        if op.oprnamespace != catalog
            || !op
                .oprcode
                .and_then(|c| snapshot.pg_proc.get(&c))
                .is_some_and(|p| p.proisstrict)
        {
            return false;
        }
    }
    any
}

fn eval(node: &protobuf::Node, h: &Hypothesis<'_>, scope: &Scope, snapshot: &PgCatalog) -> Val {
    use typedpg_pg_query::protobuf::a_const::Val as C;
    let sub = |n: &protobuf::Node| eval(n, h, scope, snapshot);
    let Some(inner) = node.node.as_ref() else {
        return Val::Any;
    };
    match inner {
        node::Node::AConst(c) if c.isnull => Val::Null,
        node::Node::AConst(c) => match &c.val {
            Some(C::Ival(i)) => Val::Num(f64::from(i.ival)),
            Some(C::Fval(f)) => f.fval.parse().map_or(Val::Any, Val::Num),
            Some(C::Boolval(b)) => Val::Bool(if b.boolval { T } else { F }),
            _ => Val::Any,
        },
        node::Node::TypeCast(c) => {
            let numeric_target = c.type_name.as_ref().is_some_and(|tn| {
                tn.typmods.is_empty()
                    && tn.array_bounds.is_empty()
                    && matches!(
                        expr::extract_string_fields(&tn.names)
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .as_slice(),
                        [
                            "pg_catalog",
                            "int2" | "int4" | "int8" | "numeric" | "float4" | "float8"
                        ]
                    )
            });
            match c.arg.as_deref().map(sub) {
                // A cast of NULL is NULL (or fails, to a domain): not TRUE.
                Some(Val::Null) => Val::Null,
                Some(v @ Val::Num(_)) if numeric_target => v,
                _ => Val::Any,
            }
        }
        node::Node::FuncCall(fc) => {
            let Some(name) =
                builtin_aggregate_name(fc, snapshot).filter(|_| is_current_level(node, scope))
            else {
                return Val::Any;
            };
            let class = aggregate_class(name);
            match h {
                Hypothesis::Empty => match class {
                    AggregateClass::Count => Val::Num(0.0),
                    AggregateClass::Hypothetical => Val::Any,
                    AggregateClass::NullKeeping | AggregateClass::Strict => Val::Null,
                },
                Hypothesis::AllNull(col) => {
                    let reads_col = aggregated_args(fc, name)
                        .into_iter()
                        .any(|a| column_of(a, scope).as_ref() == Some(*col));
                    match class {
                        // (`count(*)` reads no column.)
                        AggregateClass::Count if reads_col && !fc.agg_star => Val::Num(0.0),
                        AggregateClass::Strict if reads_col => Val::Null,
                        _ => Val::Any,
                    }
                }
            }
        }
        node::Node::AExpr(e)
            if protobuf::AExprKind::try_from(e.kind) == Ok(protobuf::AExprKind::AexprOp) =>
        {
            let op = match e.name.as_slice() {
                [n] => string_of(n),
                [s, n] if string_of(s) == Some("pg_catalog") => string_of(n),
                _ => None,
            };
            let Some(op) = op.filter(|op| builtin_strict_operator(op, snapshot)) else {
                return Val::Any;
            };
            let r = e.rexpr.as_deref().map_or(Val::Any, sub);
            let l = match e.lexpr.as_deref() {
                Some(l) => sub(l),
                // A prefix operator.
                None => {
                    return match (op, r) {
                        (_, Val::Null) => Val::Null,
                        ("-", Val::Num(x)) => Val::Num(-x),
                        ("+", Val::Num(x)) => Val::Num(x),
                        _ => Val::Any,
                    };
                }
            };
            if matches!(l, Val::Null) || matches!(r, Val::Null) {
                return Val::Null;
            }
            let (Val::Num(a), Val::Num(b)) = (l, r) else {
                return Val::Any;
            };
            let cmp = |b: bool| Val::Bool(if b { T } else { F });
            match op {
                "<" => cmp(a < b),
                "<=" => cmp(a <= b),
                ">" => cmp(a > b),
                ">=" => cmp(a >= b),
                "=" => cmp(a == b),
                "<>" | "!=" => cmp(a != b),
                "+" => Val::Num(a + b),
                "-" => Val::Num(a - b),
                "*" => Val::Num(a * b),
                _ => Val::Any,
            }
        }
        node::Node::BoolExpr(b) => {
            let masks: Vec<u8> = b.args.iter().map(|a| sub(a).mask()).collect();
            match protobuf::BoolExprType::try_from(b.boolop) {
                Ok(protobuf::BoolExprType::NotExpr) => {
                    let m = masks.first().copied().unwrap_or(T | F | N);
                    let mut out = 0;
                    if m & T != 0 {
                        out |= F;
                    }
                    if m & F != 0 {
                        out |= T;
                    }
                    if m & N != 0 {
                        out |= N;
                    }
                    Val::Bool(out)
                }
                Ok(protobuf::BoolExprType::AndExpr) => Val::Bool(fold_3vl(&masks, true)),
                Ok(protobuf::BoolExprType::OrExpr) => Val::Bool(fold_3vl(&masks, false)),
                _ => Val::Any,
            }
        }
        node::Node::NullTest(t) => {
            let Some(arg) = t.arg.as_deref() else {
                return Val::Any;
            };
            // A row-valued test (`ROW(…) IS NULL`) reads fields: unknown.
            if matches!(arg.node.as_ref(), Some(node::Node::RowExpr(_))) {
                return Val::Any;
            }
            let m = sub(arg).mask();
            let is_null_test = protobuf::NullTestType::try_from(t.nulltesttype)
                == Ok(protobuf::NullTestType::IsNull);
            let mut out = 0;
            if m & N != 0 {
                out |= if is_null_test { T } else { F };
            }
            if m & (T | F) != 0 {
                out |= if is_null_test { F } else { T };
            }
            Val::Bool(out)
        }
        _ => Val::Any,
    }
}

/// The possible outcomes of an AND (`and = true`) or OR over operands
/// with the given possible outcomes, in three-valued logic.
fn fold_3vl(masks: &[u8], and: bool) -> u8 {
    let mut acc: u8 = if and { T } else { F };
    for &m in masks {
        let mut next = 0;
        for a in [T, F, N] {
            if acc & a == 0 {
                continue;
            }
            for b in [T, F, N] {
                if m & b == 0 {
                    continue;
                }
                next |= if and {
                    match (a, b) {
                        (F, _) | (_, F) => F,
                        (N, _) | (_, N) => N,
                        _ => T,
                    }
                } else {
                    match (a, b) {
                        (T, _) | (_, T) => T,
                        (N, _) | (_, N) => N,
                        _ => F,
                    }
                };
            }
        }
        acc = next;
    }
    acc
}
